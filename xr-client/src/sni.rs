/// Extract SNI (Server Name Indication) from TLS ClientHello.
///
/// This is a lightweight parser that looks for the SNI extension in the
/// first bytes of a TCP connection. No TLS library needed.
///
/// TLS record format:
///   ContentType(1) | Version(2) | Length(2) | Fragment...
///
/// Handshake:
///   HandshakeType(1) | Length(3) | ClientHello...
///
/// ClientHello:
///   Version(2) | Random(32) | SessionID(var) | CipherSuites(var) |
///   CompressionMethods(var) | Extensions(var)
///
/// SNI extension (type 0x0000):
///   ServerNameList length(2) | NameType(1) | HostName length(2) | HostName...

/// Try to extract SNI hostname from a buffer that may contain a TLS ClientHello.
/// Returns None if the data is not TLS or doesn't contain SNI.
pub fn extract_sni(buf: &[u8]) -> Option<String> {
    // Minimum TLS record: 5 (record header) + 4 (handshake header) +
    // 2 (version) + 32 (random) + 1 (session id len) = 44
    if buf.len() < 44 {
        return None;
    }

    // TLS record header
    let content_type = buf[0];
    if content_type != 0x16 {
        // Not a Handshake record
        return None;
    }

    let record_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    let record_end = 5 + record_len.min(buf.len() - 5);

    // Handshake header (inside the record)
    let hs = &buf[5..record_end];
    if hs.is_empty() || hs[0] != 0x01 {
        // Not ClientHello
        return None;
    }

    let hs_len = ((hs[1] as usize) << 16) | ((hs[2] as usize) << 8) | (hs[3] as usize);
    let ch = &hs[4..4 + hs_len.min(hs.len() - 4)];

    // ClientHello body
    // Skip: version(2) + random(32) = 34
    if ch.len() < 35 {
        return None;
    }
    let mut pos = 34;

    // Session ID
    let session_id_len = ch[pos] as usize;
    pos += 1 + session_id_len;
    if pos + 2 > ch.len() {
        return None;
    }

    // Cipher Suites
    let cs_len = u16::from_be_bytes([ch[pos], ch[pos + 1]]) as usize;
    pos += 2 + cs_len;
    if pos + 1 > ch.len() {
        return None;
    }

    // Compression Methods
    let cm_len = ch[pos] as usize;
    pos += 1 + cm_len;
    if pos + 2 > ch.len() {
        return None;
    }

    // Extensions
    let ext_len = u16::from_be_bytes([ch[pos], ch[pos + 1]]) as usize;
    pos += 2;

    let ext_end = pos + ext_len.min(ch.len() - pos);

    while pos + 4 <= ext_end {
        let ext_type = u16::from_be_bytes([ch[pos], ch[pos + 1]]);
        let ext_data_len = u16::from_be_bytes([ch[pos + 2], ch[pos + 3]]) as usize;
        pos += 4;

        if ext_type == 0x0000 {
            // SNI extension
            return parse_sni_extension(&ch[pos..pos + ext_data_len.min(ch.len() - pos)]);
        }

        pos += ext_data_len;
    }

    None
}

fn parse_sni_extension(data: &[u8]) -> Option<String> {
    if data.len() < 5 {
        return None;
    }

    // ServerNameList length (2 bytes) — skip, parse entries
    let mut pos = 2;

    while pos + 3 <= data.len() {
        let name_type = data[pos];
        let name_len = u16::from_be_bytes([data[pos + 1], data[pos + 2]]) as usize;
        pos += 3;

        if name_type == 0x00 {
            // Host name
            if pos + name_len <= data.len() {
                return String::from_utf8(data[pos..pos + name_len].to_vec()).ok();
            }
        }

        pos += name_len;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_sni_from_real_client_hello() {
        // Minimal ClientHello with SNI for "example.com"
        let client_hello = build_test_client_hello("example.com");
        let sni = extract_sni(&client_hello);
        assert_eq!(sni, Some("example.com".to_string()));
    }

    #[test]
    fn test_not_tls() {
        let http = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert_eq!(extract_sni(http), None);
    }

    #[test]
    fn test_short_buffer() {
        assert_eq!(extract_sni(&[0x16, 0x03, 0x01]), None);
    }

    /// Build a minimal TLS ClientHello with a given SNI for testing.
    fn build_test_client_hello(hostname: &str) -> Vec<u8> {
        let host_bytes = hostname.as_bytes();

        // SNI extension
        let sni_entry_len = 3 + host_bytes.len(); // name_type(1) + len(2) + name
        let sni_list_len = sni_entry_len;
        let mut sni_ext = Vec::new();
        sni_ext.extend_from_slice(&(sni_list_len as u16).to_be_bytes()); // list len
        sni_ext.push(0x00); // host name type
        sni_ext.extend_from_slice(&(host_bytes.len() as u16).to_be_bytes());
        sni_ext.extend_from_slice(host_bytes);

        // Extensions block
        let mut extensions = Vec::new();
        extensions.extend_from_slice(&0u16.to_be_bytes()); // ext type = SNI
        extensions.extend_from_slice(&(sni_ext.len() as u16).to_be_bytes());
        extensions.extend_from_slice(&sni_ext);

        // ClientHello body
        let mut ch_body = Vec::new();
        ch_body.extend_from_slice(&[0x03, 0x03]); // TLS 1.2
        ch_body.extend_from_slice(&[0u8; 32]); // random
        ch_body.push(0); // session id length = 0
        ch_body.extend_from_slice(&2u16.to_be_bytes()); // cipher suites length
        ch_body.extend_from_slice(&[0x00, 0xff]); // one cipher suite
        ch_body.push(1); // compression methods length
        ch_body.push(0); // null compression
        ch_body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        ch_body.extend_from_slice(&extensions);

        // Handshake header
        let mut handshake = Vec::new();
        handshake.push(0x01); // ClientHello
        let hs_len = ch_body.len();
        handshake.push((hs_len >> 16) as u8);
        handshake.push((hs_len >> 8) as u8);
        handshake.push(hs_len as u8);
        handshake.extend_from_slice(&ch_body);

        // TLS record header
        let mut record = Vec::new();
        record.push(0x16); // Handshake
        record.extend_from_slice(&[0x03, 0x01]); // TLS 1.0 record version
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);

        record
    }
}
