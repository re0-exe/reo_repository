//! Phase 1 host: a console program that streams the virtual monitor to the
//! attached tablet and reconnects whenever the link comes back.

/// Parses `--fps <n>`; anything else is a usage error.
fn parse_fps(args: &[String]) -> Result<u16, String> {
    match args {
        [] => Ok(120),
        [flag, value] if flag == "--fps" => match value.parse::<u16>() {
            Ok(n) if (1..=240).contains(&n) => Ok(n),
            _ => Err(format!("Geçersiz fps değeri: {value} (1-240 arası bir sayı olmalı)")),
        },
        _ => Err("Kullanım: tablink-host [--fps <n>]".into()),
    }
}

#[cfg(windows)]
fn main() {
    use std::time::Duration;
    use tablink_core::adb::{select_device, Adb, SelectError};
    use tablink_core::session::{run_session, SessionEnd, SessionError};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let fps = match parse_fps(&args) {
        Ok(f) => f,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    tablink_core::shutdown::install_handler();
    // A run that was killed may have left its monitor attached.
    tablink_core::display::VirtualDisplay::disable_leftover();
    let adb = Adb::locate();
    println!("TabLink Host — tablet bekleniyor (çıkmak için Ctrl+C).");
    // Printed once per distinct state so the console is not flooded.
    // GPUs whose hardware encoder would not start in this run.
    let mut avoid_gpus: Vec<String> = Vec::new();
    let mut last_notice = String::new();
    let mut notice = |msg: String| {
        if msg != last_notice {
            println!("{msg}");
            last_notice = msg;
        }
    };

    while !tablink_core::shutdown::requested() {
        let devices = match adb.devices() {
            Ok(d) => d,
            Err(e) => {
                notice(format!(
                    "adb çalıştırılamadı ({e}). adb.exe dosyasını tablink-host.exe ile aynı klasöre koyun \
                     veya Android platform-tools'u PATH'e ekleyin."
                ));
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let serial = match select_device(&devices) {
            Ok(d) => d.serial.clone(),
            Err(SelectError::NoDevice) => {
                notice("Tablet bulunamadı. USB kablosunu takın ve tablette USB debugging'in açık olduğunu doğrulayın.".into());
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            Err(SelectError::Unauthorized) => {
                notice("Tablet yetki bekliyor. Tablet ekranındaki \"USB debugging'e izin ver\" penceresini onaylayın.".into());
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            Err(SelectError::Multiple(serials)) => {
                notice(format!(
                    "Birden fazla Android cihaz bağlı ({}). Yalnızca tableti bağlı bırakın.",
                    serials.join(", ")
                ));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        notice(format!("Tablet bulundu ({serial}), bağlanılıyor..."));
        match run_session(&adb, &serial, fps, &avoid_gpus) {
            SessionEnd::Disconnected => {
                notice(
                    "Bağlantı kesildi; yeniden deneniyor. (Sürekli tekrarlıyorsa tablette TabLink Display \
                     uygulamasının kurulu olduğunu doğrulayın.)"
                        .into(),
                );
                std::thread::sleep(Duration::from_secs(1));
            }
            SessionEnd::Interrupted(reason) => {
                notice(format!("Yayın kesildi ({reason}); yeniden başlatılıyor."));
                std::thread::sleep(Duration::from_secs(1));
            }
            SessionEnd::Shutdown => break,
            SessionEnd::Fatal(SessionError::NoHardwareEncoder { gpu, detail }) if !avoid_gpus.contains(&gpu) => {
                println!("'{gpu}' üzerindeki HEVC encoder başlatılamadı; diğer GPU deneniyor.\n  ({detail})");
                avoid_gpus.push(gpu);
            }
            SessionEnd::Fatal(e) => {
                eprintln!("{}", match e {
                    SessionError::DriverMissing =>
                        "Virtual Display Driver kurulu değil. Kurulum adımları: docs/driver.md".to_string(),
                    SessionError::NoHardwareEncoder { gpu, detail } => format!(
                        "Hiçbir GPU'da donanımsal HEVC encoder başlatılamadı (son denenen: {gpu}). \
                         Aşağıdaki satırı geliştiriciye iletin:\n  {detail}"
                    ),
                    SessionError::VersionMismatch =>
                        "Tablet uygulaması ile host sürümü uyuşmuyor. İkisini de aynı derlemeden kurun.".to_string(),
                    SessionError::Display(m) =>
                        format!("Sanal ekran açılamadı: {m}\nProgramı \"Run as administrator\" ile çalıştırın."),
                    SessionError::DisplayNotFound(m) => format!(
                        "Sanal monitör Windows'ta bulunamadı. Aşağıdaki raporu geliştiriciye iletin:\n{m}"
                    ),
                    SessionError::Other(m) => format!("Hata: {m}"),
                });
                std::process::exit(1);
            }
        }
    }
    println!("Kapatılıyor.");
}

#[cfg(not(windows))]
fn main() {
    eprintln!("tablink-host yalnızca Windows'ta çalışır.");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::parse_fps;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fps_defaults_to_120() {
        assert_eq!(parse_fps(&[]), Ok(120));
    }

    #[test]
    fn fps_flag_is_parsed() {
        assert_eq!(parse_fps(&args(&["--fps", "60"])), Ok(60));
    }

    #[test]
    fn fps_rejects_garbage() {
        assert!(parse_fps(&args(&["--fps", "abc"])).is_err());
        assert!(parse_fps(&args(&["--fps", "0"])).is_err());
        assert!(parse_fps(&args(&["--fps"])).is_err());
        assert!(parse_fps(&args(&["--verbose"])).is_err());
    }
}
