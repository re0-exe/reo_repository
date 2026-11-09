# TabLink Display — Aşama 1: Video Hattı Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tablet, USB üzerinden Windows'ta gerçek bir ikinci monitör olarak native çözünürlükte görüntü alır (girdi, ses, arayüz ve HDR sonraki aşamalardır).

**Architecture:** Rust host, IddCx sanal monitörünü DXGI Desktop Duplication ile yakalar, Media Foundation hardware HEVC encoder ile sıkıştırır ve `adb forward` üzerinden taşınan TCP ile Android client'a yollar; client `MediaCodec` ile `SurfaceView`'e decode eder. Wire format ayrı, platform bağımsız bir crate'tir ve iki dilde aynı golden vektörlerle test edilir.

**Tech Stack:** Rust (stable, `windows` crate), Kotlin + Jetpack Compose (Material 3), Gradle, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-11-09-usb-second-display-design.md`

## Global Constraints

- Host yalnızca Windows 11 x64; `protocol` crate'i ve `core` içindeki saf mantık Linux'ta da derlenip test edilir.
- Android `minSdk 31`, package `dev.tablink.display`.
- Çözünürlük, yenileme hızı ve codec yetenekleri sabit kodlanmaz; tabletin `Hello` mesajından gelir.
- Wire format little-endian; header 14 byte: `type u8 | flags u8 | length u32 | pts_us u64`.
- TCP port `27183`. Kanal byte'ları: control `1`, video `2`, audio `3`. Protokol sürümü `1`.
- Software encoder fallback yok; hardware encoder bulunamazsa açık hata.
- Windows API çağrıları ince modüllerde tutulur (`#[cfg(windows)]`); karar mantığı saf fonksiyonlardadır.
- Windows'a özgü kodun doğrulaması: `cargo check --target x86_64-pc-windows-msvc` (lokal) + CI `windows-latest` derlemesi. Cihaz davranışı kullanıcı testine kalır; bu, "doğrulandı" olarak raporlanmaz.
- Commit mesajlarında model adı geçmez.

## Review Focus

1. **Bozuk/aşırı büyük `length`:** 16 MiB üstü payload reddedilir, bellek ayrılmaz, bağlantı kapatılır (Task 1, 2).
2. **Mesaj ortasında kopan bağlantı (kablo çekildi):** decoder `UnexpectedEof` döner, iki taraf temiz kapanır ve yeniden bağlanmaya hazır kalır (Task 1, 6, 11).
3. **Birden fazla veya `unauthorized` adb cihazı:** tek `device` durumundaki cihaz seçilir; yoksa ayırt edici hata (Task 5).
4. **Yayın sırasında Windows'ta mod değişimi (çözünürlük/yön/yenileme):** duplication yeniden kurulur, yeni `VideoConfig` + keyframe gönderilir (Task 8, 11).
5. **Tablette uygulamanın arka plana alınması / Surface'in yok olması:** decoder serbest bırakılır; geri dönüşte `RequestKeyframe` ile devam edilir (Task 10).

---

## Dosya yapısı

```
host/Cargo.toml                         workspace
host/crates/protocol/src/{lib,header,messages}.rs
host/crates/core/src/
  lib.rs
  gate.rs            SendGate: backpressure kararı (saf)
  adb.rs             adb çıktı ayrıştırma (saf) + komut çalıştırma
  transport.rs       Transport trait, AdbTransport, kanal bağlantısı
  display.rs         VirtualDisplay (driver yapılandırma, output bulma)
  capture.rs         DesktopDuplicator
  encode/{mod,mf_hevc}.rs   VideoEncoder trait + Media Foundation impl
  session.rs         pipeline orkestrasyonu
host/crates/cli/src/main.rs             tablink-host (Aşama 1 konsol binary)
protocol-vectors/{*.bin,vectors.json}   golden vektörler
android/app/src/main/java/dev/tablink/display/
  protocol/{Header,Messages}.kt
  net/ChannelServer.kt
  video/VideoDecoder.kt
  ui/{MainActivity,StreamActivity,theme/Theme}.kt
android/app/src/test/...                JVM unit testleri
.github/workflows/build.yml
docs/device-test-phase1.md
```

---

### Task 1: `protocol` crate

**Files:** Create `host/Cargo.toml`, `host/crates/protocol/src/{lib,header,messages}.rs`, `protocol-vectors/*`

**Interfaces — Produces:**

```rust
pub const PROTOCOL_VERSION: u16 = 1;
pub const PORT: u16 = 27183;
pub const HEADER_LEN: usize = 14;
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;
pub const FLAG_KEYFRAME: u8 = 0b1;

#[repr(u8)] pub enum Channel { Control = 1, Video = 2, Audio = 3 }
#[repr(u8)] pub enum MsgType {
    Hello = 0x01, Config = 0x02, Pointer = 0x03, RequestKeyframe = 0x04,
    Ping = 0x05, Pong = 0x06, AudioRoute = 0x07,
    VideoConfig = 0x10, VideoFrame = 0x11, AudioFrame = 0x20,
}
pub struct Header { pub ty: MsgType, pub flags: u8, pub length: u32, pub pts_us: u64 }
impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN];
    pub fn decode(buf: &[u8; HEADER_LEN]) -> Result<Header, ProtocolError>;
}
pub enum ProtocolError { UnknownType(u8), PayloadTooLarge(u32), Truncated, BadVersion(u16) }

// caps bitmask: bit0 HEVC Main, bit1 Main10, bit2 4:4:4, bit3 HDR10
pub struct Hello { pub version: u16, pub width: u32, pub height: u32,
                   pub refresh_hz: Vec<u16>, pub caps: u32, pub dpi: u16 }
// codec: 0 Main 4:2:0, 1 Main10, 2 4:4:4 · profile: 0 Draw, 1 Video
pub struct Config { pub width: u32, pub height: u32, pub fps: u16, pub codec: u8,
                    pub hdr: bool, pub profile: u8, pub bitrate_kbps: u32 }
pub enum Message { Hello(Hello), Config(Config), RequestKeyframe, Ping, Pong,
                   VideoConfig(Vec<u8>), VideoFrame { keyframe: bool, data: Vec<u8> } }
pub fn write_message<W: std::io::Write>(w: &mut W, pts_us: u64, m: &Message) -> std::io::Result<()>;
pub fn read_message<R: std::io::Read>(r: &mut R) -> Result<(u64, Message), ReadError>;
// ReadError = Io(std::io::Error) | Protocol(ProtocolError)
```

`Hello` payload sırası: `version u16, width u32, height u32, n u8, n×u16, caps u32, dpi u16`.
`Config` payload sırası: struct alan sırası; `hdr` 1 byte.
`Pointer`, `AudioRoute`, `AudioFrame` tipleri enum'da tanımlıdır ama `Message` varyantları sonraki aşamalarda eklenir; `read_message` bunların payload'ını atlayıp okumaya devam eder.

- [ ] **Step 1: Failing testler** (`header.rs`, `messages.rs` içinde `#[cfg(test)]`)
  - `header_roundtrip`: `Header{VideoFrame, FLAG_KEYFRAME, 5, 0x0102030405060708}` → `encode()` == `[0x11,0x01, 5,0,0,0, 8,7,6,5,4,3,2,1]`; `decode` geri aynı.
  - `header_rejects_unknown_type`: ilk byte `0xFF` → `UnknownType(0xFF)`.
  - `header_rejects_oversize`: `length = MAX_PAYLOAD + 1` → `PayloadTooLarge`.
  - `hello_roundtrip`: `Hello{1, 2800, 1752, vec![60,120], 0b0011, 266}` yaz-oku eşit.
  - `config_roundtrip`: `Config{2800,1752,120,0,false,0,80_000}` yaz-oku eşit.
  - `hello_bad_version`: `version = 2` → `BadVersion(2)`.
  - `read_truncated_stream`: geçerli `VideoFrame`'in son byte'ı eksik → `ReadError::Io` kind `UnexpectedEof`.
  - `read_skips_unhandled_type`: `Pointer` (3 byte payload) + `Ping` → ilk dönen mesaj `Ping`.
  - `golden_vectors_match`: `protocol-vectors/vectors.json` içindeki her girdi için `write_message` çıktısı ilgili `.bin` ile byte-byte eşit.
- [ ] **Step 2:** `cd host && cargo test -p protocol` → FAIL (tanımsız).
- [ ] **Step 3:** Implementasyon. Golden vektörleri bir `#[ignore]` test (`regenerate_vectors`) üretir: `hello.bin`, `config.bin`, `video_config.bin` (payload `[0,0,0,1,0x40]`), `video_frame_key.bin` (pts `1000`, payload `[1,2,3]`), `ping.bin`; `vectors.json` her dosyanın adını ve alan değerlerini taşır.
- [ ] **Step 4:** `cargo test -p protocol` → PASS.
- [ ] **Step 5:** Commit `feat(protocol): wire format and golden vectors`.

### Task 2: Android iskeleti + Kotlin protokol codec'i

**Files:** Create `android/` Gradle projesi (`settings.gradle.kts`, `app/build.gradle.kts`, wrapper), `protocol/{Header,Messages}.kt`, `ui/theme/Theme.kt`, `ui/MainActivity.kt`, testler.

**Interfaces — Produces:**

```kotlin
object Protocol { const val VERSION = 1; const val PORT = 27183
                  const val HEADER_LEN = 14; const val MAX_PAYLOAD = 16 * 1024 * 1024
                  const val FLAG_KEYFRAME = 1 }
enum class Channel(val id: Int) { CONTROL(1), VIDEO(2), AUDIO(3) }
sealed interface Message {
  data class Hello(val version: Int, val width: Int, val height: Int,
                   val refreshHz: List<Int>, val caps: Int, val dpi: Int) : Message
  data class Config(val width: Int, val height: Int, val fps: Int, val codec: Int,
                    val hdr: Boolean, val profile: Int, val bitrateKbps: Int) : Message
  data object RequestKeyframe : Message; data object Ping : Message; data object Pong : Message
  class VideoConfig(val data: ByteArray) : Message
  class VideoFrame(val keyframe: Boolean, val data: ByteArray) : Message
}
class ProtocolException(msg: String) : java.io.IOException(msg)
fun writeMessage(out: java.io.OutputStream, ptsUs: Long, m: Message)
fun readMessage(input: java.io.InputStream): Pair<Long, Message>   // EOF → EOFException
```

Gradle: `compileSdk 35`, `minSdk 31`, Compose BOM, Material 3. Golden vektörler test kaynağı olarak `../protocol-vectors` dizininden okunur (kopyalanmaz).
`Theme.kt`: `isSystemInDarkTheme()` + `dynamicDarkColorScheme/dynamicLightColorScheme`. `MainActivity` şimdilik yalnızca "Bağlantı bekleniyor" durum ekranıdır.

- [ ] **Step 1: Failing testler** (`ProtocolTest.kt`): `goldenVectorsDecode` (her `.bin` beklenen `Message`'a çözülür), `goldenVectorsEncode` (yazım byte-byte eşit), `rejectsOversizePayload` (`length = MAX_PAYLOAD+1` → `ProtocolException`), `truncatedStreamThrowsEof`, `skipsUnhandledType`.
- [ ] **Step 2:** `cd android && ./gradlew testDebugUnitTest` → FAIL.
- [ ] **Step 3:** Implementasyon (`ByteBuffer` + `ByteOrder.LITTLE_ENDIAN`, `DataInputStream.readFully`).
- [ ] **Step 4:** `./gradlew testDebugUnitTest assembleDebug` → PASS, APK üretilir. Android SDK bu ortamda indirilemezse bu adım CI'da (Task 3) doğrulanır ve öyle raporlanır.
- [ ] **Step 5:** Commit `feat(android): project scaffold and protocol codec`.

### Task 3: CI

**Files:** Create `.github/workflows/build.yml`

- [ ] **Step 1:** İki job:
  - `host` (`windows-latest`): `cargo test --workspace` + `cargo build --release -p cli`; artifact `tablink-host.exe`.
  - `android` (`ubuntu-latest`, JDK 17): `./gradlew testDebugUnitTest assembleDebug`; artifact `app-debug.apk`.
  Tetikleyici: `push` (tüm branch'ler).
- [ ] **Step 2:** Push; GitHub MCP ile workflow run sonucunu oku → iki job yeşil.
- [ ] **Step 3:** Commit `ci: build host and android artifacts`.

Bundan sonraki her task'in son adımı push + CI yeşil kontrolüdür.

### Task 4: `SendGate` (backpressure)

**Files:** Create `host/crates/core/src/{lib,gate}.rs`

**Interfaces — Produces:**

```rust
pub struct SendGate { /* in_flight sayaç */ }
impl SendGate {
    pub fn new(max_in_flight: usize) -> Self;          // Aşama 1: 2
    pub fn try_acquire(&self) -> bool;                  // false → frame encode edilmeden atlanır
    pub fn release(&self);                              // frame sokete tam yazılınca
    pub fn dropped(&self) -> u64;
}
```

- [ ] **Step 1: Failing testler:** `acquires_up_to_limit` (2 kez `true`, 3. `false`, `dropped()==1`), `release_frees_slot`, `release_without_acquire_does_not_underflow`.
- [ ] **Step 2–4:** `cargo test -p core gate` FAIL → impl (`AtomicUsize`) → PASS.
- [ ] **Step 5:** Commit `feat(core): send gate for pre-encode frame dropping`.

### Task 5: adb + transport

**Files:** Create `host/crates/core/src/{adb,transport}.rs`

**Interfaces — Produces:**

```rust
pub enum AdbState { Device, Unauthorized, Offline }
pub struct AdbDevice { pub serial: String, pub state: AdbState }
pub fn parse_devices(output: &str) -> Vec<AdbDevice>;
pub enum SelectError { NoDevice, Unauthorized, Multiple(Vec<String>) }
pub fn select_device(devs: &[AdbDevice]) -> Result<&AdbDevice, SelectError>;

pub struct Adb { pub exe: std::path::PathBuf }          // exe yanındaki adb.exe, yoksa PATH
impl Adb {
    pub fn devices(&self) -> std::io::Result<Vec<AdbDevice>>;
    pub fn forward(&self, serial: &str, port: u16) -> std::io::Result<()>;        // önce --remove, sonra forward
    pub fn remove_forward(&self, serial: &str, port: u16) -> std::io::Result<()>;
    pub fn start_app(&self, serial: &str) -> std::io::Result<()>;                 // am start -n dev.tablink.display/.ui.StreamActivity
}
pub trait Transport { fn open(&self, ch: protocol::Channel) -> std::io::Result<std::net::TcpStream>; }
pub struct AdbTransport { pub port: u16 }               // 127.0.0.1:port, TCP_NODELAY, ilk byte = kanal id
```

`select_device` kuralı: tam bir `Device` → o; hiç `Device` yok ama `Unauthorized` var → `Unauthorized`; birden çok `Device` → `Multiple`; aksi `NoDevice`.

- [ ] **Step 1: Failing testler:**
  - `parse_devices_typical`: `"List of devices attached\nR5CX123\tdevice\nABC\tunauthorized\n\n"` → 2 cihaz, doğru state.
  - `parse_devices_ignores_daemon_noise`: `"* daemon started successfully"` satırları atlanır.
  - `select_single`, `select_unauthorized_only`, `select_multiple`, `select_none`.
  - `transport_sends_channel_byte`: lokal `TcpListener`; `AdbTransport.open(Channel::Video)` sonrası sunucu ilk byte olarak `2` okur.
- [ ] **Step 2–4:** `cargo test -p core` FAIL → impl → PASS.
- [ ] **Step 5:** Commit `feat(core): adb device selection and tcp transport`.

### Task 6: Android `ChannelServer`

**Files:** Create `net/ChannelServer.kt`, test `ChannelServerTest.kt`

**Interfaces — Produces:**

```kotlin
class ChannelServer(private val port: Int = Protocol.PORT) : java.io.Closeable {
  fun start()                                   // 127.0.0.1'e bind, SO_REUSEADDR
  suspend fun await(ch: Channel): java.net.Socket   // ilk byte'a göre eşlenmiş soket (TCP_NODELAY)
  val boundPort: Int
}
```

Geçersiz kanal byte'ı gönderen bağlantı kapatılır, diğerleri etkilenmez. Aynı kanal ikinci kez bağlanırsa eski soket kapatılıp yenisi geçerli olur (yeniden bağlanma).

- [ ] **Step 1: Failing testler** (port `0` ile): `routesByChannelByte`, `rejectsInvalidChannelByte`, `reconnectReplacesOldSocket`, `clientDisconnectMidMessageSurfacesEof` (`readMessage` → `EOFException`).
- [ ] **Step 2–4:** `./gradlew testDebugUnitTest` FAIL → impl → PASS.
- [ ] **Step 5:** Commit `feat(android): channel server`.

### Task 7: Virtual display

**Files:** Create `host/crates/core/src/display.rs`, `docs/driver.md`

- [ ] **Step 1: Driver doğrulaması (karar adımı).** Aday `VirtualDrivers/Virtual-Display-Driver` için kaynağından şunları doğrula ve `docs/driver.md`'ye yaz: lisans (yeniden dağıtıma izin veriyor mu), imzalı release var mı, özel çözünürlük/yenileme hızının nasıl yapılandırıldığı (dosya yolu ve format), yapılandırma değişikliğinin nasıl yeniden yüklendiği, HDR desteği. Kabul ölçütü: imzalı release + 2800×1752@120 yapılandırılabilir. Karşılanmazsa dur ve kullanıcıya alternatifleri bildir.
- [ ] **Step 2: Failing test** — yapılandırma üretimi saf fonksiyondur:

```rust
pub struct DisplayMode { pub width: u32, pub height: u32, pub refresh_hz: Vec<u16> }
pub fn render_driver_config(mode: &DisplayMode) -> String;   // Step 1'de belgelenen format
```

  Test `driver_config_contains_mode`: `DisplayMode{2800,1752,vec![60,120]}` çıktısı, Step 1'de belgelenen formata göre bu çözünürlüğü ve iki yenileme hızını içerir (beklenen metin `docs/driver.md`'deki örnekten alınır).
- [ ] **Step 3:** Impl + Windows katmanı:

```rust
pub struct VirtualDisplay { /* ... */ }
impl VirtualDisplay {
    pub fn is_driver_installed() -> bool;
    pub fn enable(mode: &DisplayMode) -> Result<VirtualDisplay, DisplayError>;  // config yaz, aygıtı etkinleştir
    pub fn output(&self) -> Result<IDXGIOutput1, DisplayError>;                 // sanal monitörün DXGI output'u
}
impl Drop for VirtualDisplay { /* aygıtı devre dışı bırak */ }
pub enum DisplayError { DriverMissing, EnableFailed(String), OutputNotFound }
```

- [ ] **Step 4:** `cargo test -p core display` PASS; `cargo check -p core --target x86_64-pc-windows-msvc` temiz.
- [ ] **Step 5:** Commit `feat(core): virtual display driver integration`.

### Task 8: Capture

**Files:** Create `host/crates/core/src/capture.rs`

**Interfaces — Consumes:** `VirtualDisplay::output()`. **Produces:**

```rust
pub struct CapturedFrame { pub texture: ID3D11Texture2D, pub pts_us: u64, pub width: u32, pub height: u32 }
pub enum CaptureEvent { Frame(CapturedFrame), Timeout, ModeChanged }
pub struct DesktopDuplicator { /* device, context, duplication */ }
impl DesktopDuplicator {
    pub fn new(output: &IDXGIOutput1) -> Result<Self, CaptureError>;   // output'un adapter'ında D3D11 device
    pub fn device(&self) -> &ID3D11Device;
    pub fn next(&mut self, timeout_ms: u32) -> Result<CaptureEvent, CaptureError>;
}
```

`DXGI_ERROR_ACCESS_LOST` → `ModeChanged` (çağıran yeniden `new` yapar). `DXGI_ERROR_WAIT_TIMEOUT` → `Timeout`. Yalnızca `LastPresentTime != 0` olan frame'ler döner (salt imleç güncellemeleri atlanır). `pts_us` monoton saatten.

- [ ] **Step 1: Failing test** — hata eşleme saf fonksiyonu: `fn classify(hr: i32) -> Classified` için `access_lost_maps_to_mode_changed`, `wait_timeout_maps_to_timeout`, `other_error_is_fatal`.
- [ ] **Step 2–4:** FAIL → impl → `cargo test -p core capture` PASS + Windows target `cargo check` temiz.
- [ ] **Step 5:** Commit `feat(core): desktop duplication capture`.

### Task 9: Encoder

**Files:** Create `host/crates/core/src/encode/{mod,mf_hevc}.rs`

**Interfaces — Produces:**

```rust
pub struct EncoderConfig { pub width: u32, pub height: u32, pub fps: u16, pub bitrate_kbps: u32 }
pub struct EncodedFrame { pub data: Vec<u8>, pub keyframe: bool, pub pts_us: u64 }   // Annex-B
pub trait VideoEncoder {
    fn parameter_sets(&self) -> Vec<u8>;                 // VPS+SPS+PPS, Annex-B
    fn encode(&mut self, frame: &CapturedFrame) -> Result<Option<EncodedFrame>, EncodeError>;
    fn request_keyframe(&mut self);
}
pub enum EncodeError { NoHardwareEncoder, Mf(String) }
pub struct MfHevcEncoder;   // impl VideoEncoder
impl MfHevcEncoder { pub fn new(device: &ID3D11Device, cfg: &EncoderConfig) -> Result<Self, EncodeError>; }

pub fn default_bitrate_kbps(width: u32, height: u32, fps: u16) -> u32;
pub fn split_parameter_sets(annexb: &[u8]) -> (Vec<u8>, Vec<u8>);   // (VPS/SPS/PPS, kalan)
```

`MfHevcEncoder`: `MFTEnumEx` ile `MFT_ENUM_FLAG_HARDWARE`, HEVC; `IMFDXGIDeviceManager` ile D3D11 texture girişi; BGRA→NV12 dönüşümü GPU'da `ID3D11VideoProcessor` ile; `CODECAPI_AVLowLatencyMode = true`, `CODECAPI_AVEncCommonRateControlMode = CBR`, B-frame `0`, GOP çok uzun, keyframe `CODECAPI_AVEncVideoForceKeyFrame` ile. Hardware MFT yoksa `NoHardwareEncoder`.
`default_bitrate_kbps`: `width*height*fps*0.14 / 1000`, `[20_000, 150_000]` aralığına sıkıştırılır.

- [ ] **Step 1: Failing testler:** `bitrate_for_tab_s10_plus_120hz` (2800,1752,120 → `82_414`), `bitrate_clamped_low` (640,480,30 → `20_000`), `bitrate_clamped_high` (7680,4320,120 → `150_000`), `split_parameter_sets_separates_vps_sps_pps` (NAL tipleri 32/33/34 başa, tip 19 kalana), `split_handles_3_and_4_byte_start_codes`.
- [ ] **Step 2–4:** FAIL → impl → `cargo test -p core encode` PASS + Windows target `cargo check` temiz.
- [ ] **Step 5:** Commit `feat(core): media foundation hevc encoder`.

### Task 10: Android decode + `StreamActivity`

**Files:** Create `video/VideoDecoder.kt`, `ui/StreamActivity.kt`; modify `AndroidManifest.xml`, `MainActivity.kt`

**Interfaces — Consumes:** `ChannelServer`, `readMessage`/`writeMessage`. **Produces:**

```kotlin
class VideoDecoder(private val onNeedKeyframe: () -> Unit) {
  fun configure(surface: android.view.Surface, width: Int, height: Int, fps: Int, csd: ByteArray)
  fun submit(data: ByteArray, ptsUs: Long, keyframe: Boolean)
  fun release()
}
fun buildHello(metrics: DisplayInfo, decoderCaps: Int): Message.Hello
data class DisplayInfo(val width: Int, val height: Int, val refreshHz: List<Int>, val dpi: Int)
fun landscape(info: DisplayInfo): DisplayInfo      // width >= height olacak şekilde
```

`VideoDecoder`: `video/hevc`, asenkron callback, `KEY_LOW_LATENCY=1`, `KEY_PRIORITY=0`; çıktı buffer'ları anında `releaseOutputBuffer(render=true)`. `onError` → `onNeedKeyframe()` + yeniden `configure`. İlk keyframe gelene kadar frame'ler atlanır.
`StreamActivity`: `SurfaceView`, immersive sticky, `FLAG_KEEP_SCREEN_ON`, `Surface.setFrameRate(fps, FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)`, `sensorLandscape`. Akış: `ChannelServer.start` → control bağlanınca `Hello` yaz → `Config` oku → video kanalından `VideoConfig`/`VideoFrame` döngüsü. `surfaceDestroyed` → `release()`; `surfaceCreated` → `RequestKeyframe`. Soket EOF → `MainActivity` durum ekranına dön, sunucu dinlemeye devam eder. `Ping` → `Pong`.
`decoderCaps`: `MediaCodecList` üzerinden HEVC Main (bit0), Main10 (bit1), HDR10 (bit3) sorgulanır; 4:4:4 (bit2) yalnızca codec bunu bildirirse set edilir.

- [ ] **Step 1: Failing testler** (JVM): `landscapeSwapsPortraitDimensions` (1752×2800 → 2800×1752), `helloCarriesDisplayInfo` (`DisplayInfo(2800,1752,[60,120],266)`, caps `3` → alanlar aynen), `helloUsesProtocolVersion`.
- [ ] **Step 2–4:** FAIL → impl → `./gradlew testDebugUnitTest assembleDebug` PASS.
- [ ] **Step 5:** Commit `feat(android): low-latency hevc decode and stream activity`.

### Task 11: Host session + CLI

**Files:** Create `host/crates/core/src/session.rs`, `host/crates/cli/src/main.rs`

**Interfaces — Consumes:** Task 1, 4, 5, 7, 8, 9. **Produces:**

```rust
pub fn choose_config(hello: &protocol::Hello, preferred_fps: u16) -> protocol::Config;
pub enum SessionEnd { Disconnected, Fatal(String) }
pub fn run_session(adb: &Adb, serial: &str, preferred_fps: u16) -> SessionEnd;
```

`choose_config`: fps = `preferred_fps` `hello.refresh_hz` içindeyse o, değilse listedeki en büyük değer ≤ `preferred_fps`, o da yoksa en küçük değer; codec `0`; `hdr false`; profile `0`; bitrate `default_bitrate_kbps`.
`run_session`: forward → `start_app` → control bağlan (2 sn aralıkla 10 deneme) → `Hello` oku (sürüm kontrolü) → `Config` yaz → `VirtualDisplay::enable` → duplicator → encoder → `VideoConfig` gönder → döngü: `next(…)`; `Frame` ise `gate.try_acquire()` → encode → gönderim thread'i yazar ve `release()`; `ModeChanged` ise duplicator + encoder yeniden kurulur, yeni `VideoConfig` + keyframe. Control thread'i: `RequestKeyframe` → `encoder.request_keyframe()`; saniyede bir `Ping`, `Pong` gecikmesi loglanır. Herhangi bir soket hatası/EOF → `Disconnected` (display drop ile kaldırılır, forward silinir).
`main.rs`: `--fps <n>` (varsayılan `120`). Sonsuz döngü: `select_device` → `run_session`; `Disconnected` sonrası 1 sn bekleyip yeniden dener. `Unauthorized`, `Multiple`, `DriverMissing`, `NoHardwareEncoder` için ayrı, eyleme dönük Türkçe mesaj basar.

- [ ] **Step 1: Failing testler:** `config_prefers_requested_fps` (hz `[60,120]`, 120 → 120), `config_falls_back_to_lower` (hz `[60,90]`, 120 → 90), `config_falls_back_to_min` (hz `[144]`, 120 → 144), `config_uses_hello_resolution`.
- [ ] **Step 2–4:** FAIL → impl → `cargo test --workspace` PASS + Windows target `cargo check --workspace` temiz.
- [ ] **Step 5:** Commit `feat: host session pipeline and cli`; push; CI yeşil.

### Task 12: Cihaz test rehberi

**Files:** Create `docs/device-test-phase1.md`, `README.md`

- [ ] **Step 1:** Türkçe, numaralı, kısa adımlar: (1) CI artifact'lerini indirme, (2) driver kurulumu (`docs/driver.md`'ye göre), (3) tablette USB debugging açma ve yetkilendirme, (4) APK kurulumu (`adb install`), (5) `tablink-host.exe` çalıştırma, (6) kontrol listesi: Settings → Display'de ikinci monitör görünüyor mu · çözünürlük 2800×1752 mi · 120 Hz seçilebiliyor mu · extend/duplicate çalışıyor mu · konsoldaki ping değeri · kablo çıkar-tak sonrası yeniden bağlanıyor mu · Windows'ta çözünürlük/yön değişince görüntü toparlanıyor mu. Her madde için "sorun olursa bana şunu gönderin" (konsol çıktısı).
- [ ] **Step 2:** Commit `docs: phase 1 device test guide`; push.
