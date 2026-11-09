# TabLink Display

Samsung Galaxy Tab S10+ tableti, Windows 11 bilgisayara **USB üzerinden**
bağlı ikinci monitör olarak kullanmak için.

| Parça | Konum | Teknoloji |
|---|---|---|
| Windows host | `host/` | Rust, DXGI Desktop Duplication, Media Foundation HEVC |
| Android client | `android/` | Kotlin, Jetpack Compose, MediaCodec |
| Wire format | `host/crates/protocol`, `protocol-vectors/` | İki tarafta aynı test vektörleri |

**Durum:** Aşama 1 (video hattı) yazıldı; gerçek cihazda henüz denenmedi.

- Tasarım: [`docs/superpowers/specs/2026-11-09-usb-second-display-design.md`](docs/superpowers/specs/2026-11-09-usb-second-display-design.md)
- Kurulum ve test: [`docs/device-test-phase1.md`](docs/device-test-phase1.md)
- Driver: [`docs/driver.md`](docs/driver.md)

## Geliştirme

```
cd host && cargo test --workspace          # platform bağımsız testler
cd android && ./gradlew testDebugUnitTest  # Kotlin testleri
```

Her push'ta GitHub Actions `tablink-host.exe` ve `app-debug.apk` üretir.
