//! Прямой utun-интерфейс без Network Extension (XR-309). Устройство живёт,
//! пока открыт сокет ядра: аварийное закрытие файла убирает интерфейс и
//! зависящие от него маршруты, kill-switch в PF при этом остаётся.
//!
//! Путь устройства: сокет `AF_SYSTEM/SYSPROTO_CONTROL`, ioctl `CTLIOCGINFO`
//! достаёт идентификатор контроллера по имени `com.apple.net.utun_control`,
//! connect с sc_unit=0 просит ядро выдать свободный номер, сокет-опция
//! `UTUN_OPT_IFNAME` отдаёт имя интерфейса. Константы не экспортируются в
//! публичных заголовках, значения из if_utun.c и kern_control.h.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

use crate::frame;

const AF_SYSTEM: libc::c_int = 32;
const AF_SYS_CONTROL: u16 = 2;
const SYSPROTO_CONTROL: libc::c_int = 2;
const UTUN_CONTROL_NAME: &[u8] = b"com.apple.net.utun_control";
const UTUN_OPT_IFNAME: libc::c_int = 2;
const MAX_KCTL_NAME: usize = 96;
const CTLIOCGINFO: libc::c_ulong = 0xc0644e03; // _IOWR('N', 3, struct ctl_info)

#[repr(C)]
struct CtlInfo {
    ctl_id: u32,
    ctl_name: [u8; MAX_KCTL_NAME],
}

#[repr(C)]
struct SockaddrCtl {
    sc_len: u8,
    sc_family: u8,
    ss_sysaddr: u16,
    sc_id: u32,
    sc_unit: u32,
    sc_reserved: [u32; 5],
}

pub struct Utun {
    fd: OwnedFd,
    pub ifname: String,
}

impl Utun {
    /// Открыть новый utun. Требует прав root.
    pub fn open() -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                AF_SYSTEM,
                libc::SOCK_DGRAM,
                SYSPROTO_CONTROL,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Отрицательный fd отсечён выше, дальше только валидный дескриптор.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let mut info = CtlInfo { ctl_id: 0, ctl_name: [0; MAX_KCTL_NAME] };
        info.ctl_name[..UTUN_CONTROL_NAME.len()]
            .copy_from_slice(UTUN_CONTROL_NAME);
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), CTLIOCGINFO, &mut info) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        let addr = SockaddrCtl {
            sc_len: std::mem::size_of::<SockaddrCtl>() as u8,
            sc_family: AF_SYSTEM as u8,
            ss_sysaddr: AF_SYS_CONTROL,
            sc_id: info.ctl_id,
            sc_unit: 0,
            sc_reserved: [0; 5],
        };
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                &addr as *const SockaddrCtl as *const libc::sockaddr,
                std::mem::size_of::<SockaddrCtl>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut ifname = [0u8; libc::IF_NAMESIZE];
        let mut len = ifname.len() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                SYSPROTO_CONTROL,
                UTUN_OPT_IFNAME,
                ifname.as_mut_ptr() as *mut libc::c_void,
                &mut len,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let name = std::ffi::OsStr::from_bytes(&ifname[..len.saturating_sub(1) as usize])
            .to_string_lossy()
            .into_owned();

        Ok(Self { fd, ifname: name })
    }

    /// Прочитать один кадр. Блокирующий сокет, вызывается из blocking-треда.
    /// Ok(None) значит прерывание сигналом, вызывающий повторяет попытку.
    /// Нулевая длина чтения это смерть интерфейса: молчаливый возврат
    /// раскрутил бы петлю чтения вхолостую, поэтому уходит ошибкой.
    pub fn read_frame(&self, buf: &mut [u8]) -> io::Result<Option<(Vec<u8>, u32)>> {
        let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(err);
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "интерфейс utun закрылся",
            ));
        }
        Ok(frame::decode(&buf[..n as usize]))
    }

    /// Записать кадр с заголовком семейства.
    pub fn write_packet(&self, packet: &[u8], family: u32) -> io::Result<()> {
        let frame = frame::encode(packet, family);
        let mut off = 0usize;
        while off < frame.len() {
            let n = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    frame[off..].as_ptr() as *const libc::c_void,
                    frame.len() - off,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            off += n as usize;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_darwin_headers() {
        // Значения сверены с sys/socket.h, sys/sys_domain.h и if_utun.c;
        // тест держит их рядом с кодом, который их использует.
        assert_eq!(AF_SYSTEM, 32);
        assert_eq!(AF_SYS_CONTROL, 2);
        assert_eq!(SYSPROTO_CONTROL, 2);
        assert_eq!(UTUN_OPT_IFNAME, 2);
        assert_eq!(MAX_KCTL_NAME, 96);
        assert_eq!(CTLIOCGINFO, 0xc0644e03);
        // struct ctl_info и sockaddr_ctl из kern_control.h.
        assert_eq!(std::mem::size_of::<CtlInfo>(), 100);
        assert_eq!(std::mem::size_of::<SockaddrCtl>(), 32);
    }
}
