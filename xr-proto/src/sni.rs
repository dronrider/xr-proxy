use crate::protocol::MAX_DOMAIN_LEN;

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

/// Заголовок TLS-рекорда: тип, версия и длина.
pub const TLS_RECORD_HEADER_LEN: usize = 5;

/// Дальше этого объёма первый рекорд не растёт: полезная часть TLSPlaintext
/// ограничена 2^14 байтами. Предел нужен, чтобы соврамшая длина в заголовке не
/// заставила прокси ждать байты, которых клиент не пришлёт.
pub const MAX_CLIENT_HELLO: usize = TLS_RECORD_HEADER_LEN + 16384;

/// Сколько всего байт должно набраться в буфере, чтобы первый рекорд лежал в
/// нём целиком. `None` значит, что дочитывать нечего: это не handshake.
///
/// Съём имени с одной порции первых байт работал, пока ClientHello влезал в
/// сегмент. Постквантовый обмен ключами (`X25519MLKEM768`) раздул key_share
/// больше чем на килобайт, и рекорд поехал по нескольким сегментам: разбор
/// видит обрезанное начало, `extract_sni` молча отдаёт `None`, а соединение
/// уходит действием по умолчанию. Длина из заголовка и говорит, сколько ещё
/// ждать.
pub fn client_hello_record_len(buf: &[u8]) -> Option<usize> {
    if buf.first() != Some(&0x16) {
        return None;
    }
    if buf.len() < TLS_RECORD_HEADER_LEN {
        return Some(TLS_RECORD_HEADER_LEN);
    }
    let record_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    Some((TLS_RECORD_HEADER_LEN + record_len).min(MAX_CLIENT_HELLO))
}

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
    // Заголовок handshake это 4 байта. Обрезанный рекорд (длина в заголовке
    // меньше самого заголовка) раньше уводил чтение за границу среза (XR-205).
    if hs.len() < 4 {
        // Уровень warn, а не info: дефолтный log_level это тоже warn, и уровнем
        // ниже срабатывание защиты на роутере с нетронутым конфигом никто бы не
        // увидел.
        tracing::warn!(
            "SNI: рекорд короче заголовка handshake ({} байт), пропускаем",
            hs.len()
        );
        return None;
    }
    if hs[0] != 0x01 {
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
            // Host name. Имя длиннее байтового префикса Connect на провод не
            // лезет, а настоящих таких доменов не бывает (DNS даёт максимум
            // 253 символа): такое соединение уходит маршрутом по IP.
            if name_len > MAX_DOMAIN_LEN {
                tracing::warn!(
                    "SNI: имя {} байт, больше предела домена, пропускаем",
                    name_len
                );
                return None;
            }
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

    /// Рекорд объявляет длину меньше заголовка handshake: чтение заголовка
    /// уходило за границу среза и роняло процесс (XR-205). Буфер при этом
    /// длиннее минимальных 44 байт, иначе разбор отсекается раньше.
    #[test]
    fn test_truncated_record_len() {
        for record_len in 0u16..=4 {
            let mut buf = vec![0x16, 0x03, 0x01];
            buf.extend_from_slice(&record_len.to_be_bytes());
            buf.push(0x01); // ClientHello, но рекорд обрывается на нём
            buf.resize(64, 0);
            assert_eq!(extract_sni(&buf), None, "record_len={}", record_len);
        }
    }

    /// SNI длиннее байтового префикса Connect не поедет на провод, поэтому
    /// сниффер отдаёт None и соединение уходит маршрутом по IP.
    #[test]
    fn test_overlong_sni_ignored() {
        let long = "a".repeat(MAX_DOMAIN_LEN + 1);
        assert_eq!(extract_sni(&build_test_client_hello(&long)), None);

        // Ровно на границе имя ещё принимается.
        let edge = "a".repeat(MAX_DOMAIN_LEN);
        assert_eq!(extract_sni(&build_test_client_hello(&edge)), Some(edge));
    }

    /// Настоящий браузерный ClientHello непохож на минимальную фикстуру: в нём
    /// возобновляемый session_id, десятки шифров и SNI где-то посреди чужих
    /// расширений. Ровно эти поля разбор пропускает арифметикой, поэтому
    /// проверяем её на таком образце (замечание ревью).
    #[test]
    fn test_extract_sni_from_rich_client_hello() {
        let hello = build_rich_client_hello("news.example.org");
        assert_eq!(extract_sni(&hello), Some("news.example.org".to_string()));
    }

    /// ClientHello, разорванный на TCP-сегменты: в буфере лежит начало, а
    /// заголовок рекорда обещает больше. Разбор обязан выжить на любом обрыве.
    #[test]
    fn test_record_longer_than_buffer() {
        let hello = build_rich_client_hello("news.example.org");
        for cut in 1..hello.len() {
            let part = &hello[..cut];
            match extract_sni(part) {
                None => {}
                Some(name) => assert_eq!(name, "news.example.org", "обрыв на {}", cut),
            }
        }
    }

    /// По заголовку рекорда видно, сколько байт ждать. Не-handshake и пустой
    /// буфер ждать не заставляют, иначе прокси висел бы на каждом сыром TCP.
    #[test]
    fn test_record_len_from_header() {
        let hello = build_rich_client_hello("news.example.org");
        assert_eq!(client_hello_record_len(&hello), Some(hello.len()));
        // Первый сегмент короче рекорда: ждать надо всю длину из заголовка.
        assert_eq!(client_hello_record_len(&hello[..40]), Some(hello.len()));
        // Заголовок ещё не набрался, но начало handshake уже видно.
        assert_eq!(client_hello_record_len(&hello[..2]), Some(TLS_RECORD_HEADER_LEN));

        assert_eq!(client_hello_record_len(b""), None);
        assert_eq!(client_hello_record_len(b"GET / HTTP/1.1\r\n"), None);
    }

    /// Соврамший заголовок не должен превращать ожидание в бесконечное: рекорд
    /// длиннее предела TLSPlaintext обрезается по нему.
    #[test]
    fn test_record_len_capped() {
        let buf = [0x16, 0x03, 0x01, 0xff, 0xff];
        assert_eq!(client_hello_record_len(&buf), Some(MAX_CLIENT_HELLO));
    }

    /// Крупный ClientHello (в жизни его раздувает постквантовый key_share) не
    /// влезает в сегмент: на первой половине имени нет, на собранном рекорде
    /// оно достаётся.
    #[test]
    fn test_sni_needs_full_record() {
        let hello = build_large_client_hello("news.example.org");
        let cut = 1400.min(hello.len() / 2);
        assert_eq!(extract_sni(&hello[..cut]), None);
        assert_eq!(client_hello_record_len(&hello[..cut]), Some(hello.len()));
        assert_eq!(
            extract_sni(&hello),
            Some("news.example.org".to_string())
        );
    }

    /// Build a minimal TLS ClientHello with a given SNI for testing.
    fn build_test_client_hello(hostname: &str) -> Vec<u8> {
        build_client_hello(hostname, 0, 1, false, 0)
    }

    /// То же, но с полями, которые есть у живого клиента.
    fn build_rich_client_hello(hostname: &str) -> Vec<u8> {
        build_client_hello(hostname, 32, 30, true, 0)
    }

    /// Живой браузерный ClientHello с постквантовым обменом ключами: key_share
    /// весит больше килобайта, стоит до SNI и выносит имя за первый сегмент.
    fn build_large_client_hello(hostname: &str) -> Vec<u8> {
        build_client_hello(hostname, 32, 30, true, 2048)
    }

    /// `session_id_len` и `cipher_count` набивают поля, которые разбор
    /// перепрыгивает по длине. При `padded` вокруг SNI встают чужие расширения,
    /// а в ServerNameList первой идёт запись не-host_name. `key_share_len` это
    /// вес расширения, которое лежит перед SNI.
    fn build_client_hello(
        hostname: &str,
        session_id_len: usize,
        cipher_count: usize,
        padded: bool,
        key_share_len: usize,
    ) -> Vec<u8> {
        let host_bytes = hostname.as_bytes();

        // SNI extension
        let mut entries = Vec::new();
        if padded {
            // name_type 0x02 (историческая host_name_srv), разбор её пропускает
            entries.push(0x02);
            entries.extend_from_slice(&4u16.to_be_bytes());
            entries.extend_from_slice(b"skip");
        }
        entries.push(0x00); // host name type
        entries.extend_from_slice(&(host_bytes.len() as u16).to_be_bytes());
        entries.extend_from_slice(host_bytes);

        let mut sni_ext = Vec::new();
        sni_ext.extend_from_slice(&(entries.len() as u16).to_be_bytes()); // list len
        sni_ext.extend_from_slice(&entries);

        // Extensions block
        let mut extensions = Vec::new();
        if padded {
            // supported_versions до SNI
            extensions.extend_from_slice(&0x002bu16.to_be_bytes());
            extensions.extend_from_slice(&3u16.to_be_bytes());
            extensions.extend_from_slice(&[0x02, 0x03, 0x04]);
        }
        if key_share_len > 0 {
            extensions.extend_from_slice(&0x0033u16.to_be_bytes()); // key_share
            extensions.extend_from_slice(&(key_share_len as u16).to_be_bytes());
            extensions.extend(std::iter::repeat(0xa5).take(key_share_len));
        }
        extensions.extend_from_slice(&0u16.to_be_bytes()); // ext type = SNI
        extensions.extend_from_slice(&(sni_ext.len() as u16).to_be_bytes());
        extensions.extend_from_slice(&sni_ext);
        if padded {
            // padding после SNI
            extensions.extend_from_slice(&0x0015u16.to_be_bytes());
            extensions.extend_from_slice(&64u16.to_be_bytes());
            extensions.extend_from_slice(&[0u8; 64]);
        }

        // ClientHello body
        let mut ch_body = Vec::new();
        ch_body.extend_from_slice(&[0x03, 0x03]); // TLS 1.2
        ch_body.extend_from_slice(&[0u8; 32]); // random
        ch_body.push(session_id_len as u8);
        ch_body.extend(std::iter::repeat(0x5a).take(session_id_len));
        ch_body.extend_from_slice(&((cipher_count * 2) as u16).to_be_bytes());
        for i in 0..cipher_count {
            ch_body.extend_from_slice(&(0x1300u16 + i as u16).to_be_bytes());
        }
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
