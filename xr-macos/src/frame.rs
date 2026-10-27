//! Кадр utun: ядро кладёт перед каждым IP-пакетом заголовок с семейством
//! адресов, и тот же заголовок ждёт на записи. Заголовок это `u_int32_t`
//! в порядке байтов машины, для IPv4 приходит 2, для IPv6 30. Утилиты
//! разных авторов пишут оба порядка, поэтому чтение разбирает оба и ещё
//! восьмибайтовый вариант IPv6 с меткой потока (packet info mode), а
//! запись ведёт родным порядком машины, как делает ядро.

pub const AF_INET_DARWIN: u32 = 2;
pub const AF_INET6_DARWIN: u32 = 30;

/// Обёртка для записи: заголовок семейства + IP-пакет.
pub fn encode(packet: &[u8], family: u32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + packet.len());
    frame.extend_from_slice(&family.to_ne_bytes());
    frame.extend_from_slice(packet);
    frame
}

/// Разобрать кадр из utun. Возвращает пакет без заголовка и семейство.
/// Неизвестные кадры (не IP, кривой заголовок) отбрасываются как None:
/// потерять один кадр дешевле, чем уронить цикл чтения.
pub fn decode(frame: &[u8]) -> Option<(Vec<u8>, u32)> {
    let packet = strip_header(frame)?;
    let family = match packet.first()? >> 4 {
        4 => AF_INET_DARWIN,
        6 => AF_INET6_DARWIN,
        _ => return None,
    };
    Some((packet.to_vec(), family))
}

fn strip_header(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() < 5 {
        return None;
    }
    let be = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
    // Заголовок родным порядком (маленький endian на Darwin): 02 00 00 00.
    if frame[0] == (AF_INET_DARWIN as u8) && frame[1..4] == [0, 0, 0] {
        return Some(&frame[4..]);
    }
    if frame[0] == (AF_INET6_DARWIN as u8) && frame[1..4] == [0, 0, 0] {
        // Пакет сразу за заголовком: обычный четырёхбайтовый вариант.
        if frame[4] >> 4 == 6 {
            return Some(&frame[4..]);
        }
        // Восьмибайтовый вариант IPv6 с меткой потока: после семейства лежит
        // метка, пакет начинается с восьмого байта и обязан быть IPv6.
        if frame.len() > 8 && frame[8] >> 4 == 6 {
            return Some(&frame[8..]);
        }
        return None;
    }
    // Тот же заголовок большим порядком: 00 00 00 02 / 00 00 00 1e.
    if be == AF_INET_DARWIN || be == AF_INET6_DARWIN {
        return Some(&frame[4..]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_prepends_native_order_family() {
        let frame = encode(&[0x45, 0, 0, 0], AF_INET_DARWIN);
        assert_eq!(frame, vec![2, 0, 0, 0, 0x45, 0, 0, 0]);
    }

    #[test]
    fn decode_strips_native_order_header() {
        let pkt = vec![0x45, 1, 2, 3, 4];
        let frame = encode(&pkt, AF_INET_DARWIN);
        assert_eq!(decode(&frame), Some((pkt, AF_INET_DARWIN)));
    }

    #[test]
    fn decode_strips_big_endian_header() {
        let pkt = vec![0x60, 9, 8, 7];
        let frame = [0, 0, 0, 30, 0x60, 9, 8, 7];
        assert_eq!(decode(&frame), Some((pkt, AF_INET6_DARWIN)));
    }

    #[test]
    fn decode_strips_flowinfo_header() {
        let pkt = vec![0x60, 1, 1, 1, 1];
        let frame = [30, 0, 0, 0, 0, 0, 0, 1, 0x60, 1, 1, 1, 1];
        assert_eq!(decode(&frame), Some((pkt, AF_INET6_DARWIN)));
    }

    #[test]
    fn decode_rejects_non_ip_and_short_frames() {
        assert_eq!(decode(&[2, 0, 0, 0]), None);
        assert_eq!(decode(&[9, 0, 0, 0, 0x40]), None);
        assert_eq!(decode(&[]), None);
    }

    #[test]
    fn decode_roundtrip_both_families() {
        for (family, first) in [(AF_INET_DARWIN, 0x45u8), (AF_INET6_DARWIN, 0x60u8)] {
            let pkt = vec![first, 1, 2, 3];
            assert_eq!(decode(&encode(&pkt, family)), Some((pkt, family)));
        }
    }
}
