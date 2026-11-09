# TabLink Display — USB İkinci Ekran: Tasarım (Spec)

Tarih: 2026-11-09 · Durum: kullanıcı incelemesi bekliyor

## 1. Amaç

Samsung Galaxy Tab S10+ tableti, Windows 11 laptopa **USB kablosu üzerinden**
bağlı gerçek bir ikinci monitör olarak kullanmak. Referans: Samsung Second
Screen (Wi-Fi). Hedef, onun özelliklerini eksiksiz karşılamak ve Wi-Fi
kaynaklı gecikmeyi ortadan kaldırmak.

**Kullanım öncelikleri (kullanıcıdan):** ders/üretkenlik, S Pen ile çizim/not,
video izleme. Oyun kapsam dışı.

## 2. Donanım ve kısıtlar

| Cihaz | Özellik |
|---|---|
| Laptop | ASUS ROG Strix G513IC — Ryzen 7 4800H, RTX 3050, 32 GB RAM, Windows 11 |
| Tablet | Galaxy Tab S10+ — 12.4", 2800×1752, 120 Hz, HDR destekli AMOLED |

Tablet değerleri (çözünürlük, USB hızı, HDR, decoder yetenekleri) varsayımdır;
uygulama bunları çalışma anında cihazdan sorgular, sabit kodlanmaz.

- Ham görüntü (2800×1752, 24 bpp, 120 Hz ≈ 14,1 Gbps) USB bant genişliğine
  sığmaz; sıkıştırma zorunludur. Hedef "visually lossless".
- Geliştirme ortamında Windows/tablet donanımı yoktur. Doğrulama: unit testler
  + GitHub Actions derlemesi + kullanıcının cihaz testi.

## 3. Başarı ölçütleri

1. Tablet, Windows **Settings → Display** içinde normal bir monitör olarak
   görünür: extend/duplicate, konum, ölçek, yön, yenileme hızı oradan yönetilir.
2. Native 2800×1752, ölçekleme yok; 60 ve 120 Hz seçilebilir.
3. Uçtan uca gecikme hedefi ≤ 30 ms (Çizim profili). Uygulama içi gecikme
   göstergesi ile ölçülür.
4. Touch ve S Pen (basınç, eğim, hover, yan tuş, silgi) Windows'ta gerçek
   pointer girdisi olarak çalışır.
5. Sistem sesi isteğe bağlı olarak tablet hoparlöründen çalınır.
6. Kablo takıldığında bağlantı kullanıcı müdahalesi olmadan kurulur.
7. İki arayüz de sistem açık/koyu temasını izler.
8. HDR (genişletilmiş hedef, Aşama 5): Windows'ta "Use HDR" açılabilir ve
   tablet HDR olarak gösterir.

## 4. Mimari

```
Windows host                                 Android client
IddCx virtual display driver
  → DXGI Desktop Duplication (GPU texture)
  → HEVC hardware encoder        ──video──▶  MediaCodec → SurfaceView
WASAPI loopback (PCM 48 kHz)     ──audio──▶  AudioTrack
InjectSyntheticPointerInput      ◀─control─  MotionEvent (touch / stylus)
                    USB: adb forward ile taşınan TCP
```

### 4.1 Repo yapısı

```
host/                 Rust workspace
  crates/protocol     Wire format, platform bağımsız, unit testli
  crates/core         display, capture, encode, audio, input, transport
  crates/app          Tauri 2 arayüzü + system tray
android/              Kotlin, Jetpack Compose, Material 3
docs/
.github/workflows/    Windows .exe ve Android .apk derlemesi
```

### 4.2 Transport

- Android uygulaması `localhost` üzerinde dinler; host, paketlenmiş `adb.exe`
  ile `adb forward` kurar ve bağlanır.
- Üç ayrı TCP bağlantısı: **video**, **audio**, **control**. İlk byte kanal
  türünü belirtir. Ayrı bağlantılar, bir kanalın diğerini bloklamasını önler.
- Transport, `Transport` trait'i arkasındadır; ileride AOA eklenebilir.

### 4.3 Wire format (`protocol`)

Her mesaj: `type: u8 | flags: u8 | length: u32 | pts_us: u64 | payload`
(little-endian).

| Kanal | Mesajlar |
|---|---|
| control | `Hello` (sürüm, cihaz yetenekleri), `Config` (çözünürlük, fps, codec, HDR, profil), `Pointer` (touch/pen), `RequestKeyframe`, `Ping`/`Pong`, `AudioRoute` |
| video | `VideoConfig` (VPS/SPS/PPS), `VideoFrame` (flags: keyframe) |
| audio | `AudioFrame` (PCM s16le, 48 kHz, stereo) |

Rust ve Kotlin codec'leri aynı golden test vektörleriyle doğrulanır.

### 4.4 Windows host

- **Virtual display:** imzalı, açık kaynak bir IddCx driver paketlenir
  (aday: VirtualDrivers/Virtual-Display-Driver; lisans ve HDR desteği plan
  aşamasında doğrulanacak). Host, tabletin `Hello` mesajındaki çözünürlük ve
  yenileme hızlarını driver yapılandırmasına yazar. Sıfırdan driver yazmak
  kapsam dışıdır.
- **Capture:** DXGI Desktop Duplication. Yalnızca değişen frame'ler gelir;
  statik ekranda veri gönderilmez.
- **Encode:** `VideoEncoder` trait'i. İlk implementasyon: Media Foundation
  hardware HEVC encoder (RTX 3050 / NVENC), D3D11 texture girişi (zero-copy),
  low-latency mod, B-frame yok, uzun GOP + istek üzerine keyframe.
  4:4:4 veya HDR için yetersiz kalırsa aynı trait arkasına doğrudan NVENC
  implementasyonu eklenir.
- **Chroma:** tablet decoder'ı 4:4:4 destekliyorsa kullanılır; değilse 4:2:0.
- **Backpressure:** gönderim kuyruğu dolarsa frame, encode edilmeden önce
  atlanır (referans zinciri bozulmaz).
- **Input:** `InjectSyntheticPointerInput` ile `PT_TOUCH` (çoklu dokunma) ve
  `PT_PEN` (basınç, eğim, hover, barrel button, eraser). Koordinatlar sanal
  monitörün masaüstü dikdörtgenine eşlenir. Pen menzildeyken avuç içi
  dokunuşları yok sayılır (ayarlanabilir).
- **Audio:** WASAPI loopback ile varsayılan çıkış yakalanır. "Tablet" seçiliyken
  PC hoparlörü susturulur.
- **Otomatik bağlantı:** adb ile cihaz izlenir → `adb forward` → uygulama
  `am start` ile açılır → bağlanılır. Kablo çıkınca sanal monitör kaldırılır.

### 4.5 Android client

- minSdk 31. `MediaCodec` asenkron mod, `KEY_LOW_LATENCY`, çıkış `SurfaceView`.
- Tam ekran immersive, ekran açık tutulur, 120 Hz frame rate isteği.
- `MotionEvent` → `Pointer` mesajları; batched historical event'ler dahil
  (S Pen örnekleme hızını korumak için).
- `AudioTrack` low-latency modda.
- Bağlantı yokken sade bir durum ekranı: kurulum adımları (USB debugging),
  bağlantı durumu.

### 4.6 Kalite profilleri

| Profil | Davranış |
|---|---|
| Çizim / Üretkenlik | Frame gelir gelmez gösterilir; ses buffer'ı minimum |
| Video | Frame'ler PTS'e göre sunulur, ses ile senkron; küçük jitter buffer |

### 4.7 Arayüz

- **Windows:** system tray ikonu + tek ayar penceresi. İçerik: bağlantı durumu,
  yenileme hızı, profil, ses çıkışı, gecikme göstergesi, "Display settings"
  kısayolu. Windows 11 görünümüne uygun, sistem temasını izler.
- **Android:** Material 3, dynamic color, açık/koyu. 12.4" 16:10 yatay ve
  dikey düzen. Yayın sırasında arayüz yoktur; kenardan kaydırma ile küçük bir
  overlay (profil, ses, bağlantıyı kes) açılır.

## 5. Hata durumları

| Durum | Davranış |
|---|---|
| USB debugging kapalı / yetkisiz | Host penceresinde adım adım yönerge |
| Driver kurulu değil | Yönetici izniyle kurulum akışı |
| Hardware encoder yok | Açık hata mesajı; software fallback yok (gecikme hedefini bozar) |
| Decoder hatası / bozuk frame | Client `RequestKeyframe` gönderir |
| Kablo çıkması | İki taraf da temiz kapanır, yeniden takılınca otomatik bağlanır |
| Protokol sürüm uyuşmazlığı | `Hello` aşamasında reddedilir, güncelleme uyarısı |

## 6. Test stratejisi

- `protocol`: unit testler + paylaşılan golden vektörler (Rust ve Kotlin).
- `core`: platform bağımsız mantık (koordinat eşleme, backpressure, profil
  zamanlaması) unit testli; Windows API çağrıları ince bir katmanda tutulur.
- CI: her push'ta `windows-latest` üzerinde host derlemesi ve testleri,
  `ubuntu-latest` üzerinde APK derlemesi ve testleri; çıktılar indirilebilir.
- Cihaz testi: her aşama sonunda kullanıcıya kısa bir kontrol listesi.

## 7. Aşamalar

1. **Video hattı:** protokol, transport, virtual display, capture, encode,
   decode. Çıktı: tablet ikinci monitör olarak görüntü verir.
2. **Girdi:** touch + S Pen.
3. **Ses:** tablet hoparlörü seçeneği, A/V sync.
4. **Arayüz ve cila:** profiller, otomatik bağlantı, tray, temalar, kurulum.
5. **HDR:** IddCx HDR modu, 10-bit HEVC (Main10, BT.2020 PQ), tablet HDR çıkışı.

Her aşama ayrı bir implementation plan alır ve cihazda denenebilir bir sürümle
biter.

## 8. Açık riskler

- **4:4:4 decode** tablet donanımında muhtemelen yok → 4:2:0 fallback.
- **HDR** üç koşula bağlı: driver'ın HDR desteği, encoder'ın 10-bit girişi,
  tabletin HDR10 decode'u. Biri eksikse SDR'da kalınır; bu yüzden son aşamadır.
- **PC hoparlörünü susturma:** endpoint mute iken loopback'in sessiz gelip
  gelmediği sürücüye bağlı olabilir; cihazda doğrulanacak. Gerekirse sanal ses
  aygıtına geçilir.
- **Optimus:** dahili ekran iGPU'da; sanal monitörün RTX üzerinde render ve
  encode edilmesi cihazda doğrulanacak.
- **DRM içerik** (Netflix vb.) capture'da siyah görünür; çözülemez.
- **ADB throughput:** hedef bitrate (50–150 Mbps) için yeterli olması
  bekleniyor; Aşama 1'de ölçülecek.

## 9. Kapsam dışı

Oyun odaklı optimizasyon, Wi-Fi bağlantısı, AOA transport, Windows dışı host,
tablet klavyesinin PC'ye aktarımı, kendi driver'ımızı yazmak.
