# Durum (2026-11-11) — proje beklemede

Aşama 1 (video hattı) gerçek donanımda kısmen doğrulandı, sonra durduruldu.

## Cihazda çalıştığı görülenler

- adb bağlantısı, el sıkışma, tabletin çözünürlük/yenileme hızı bildirimi
- Sanal monitörün açılması, Windows'ta 2800×1752 @ 120 Hz görünmesi
- Yakalama → HEVC encode → USB → tablette decode ve gösterim (keskin görüntü)
- Encoder başlamazsa diğer GPU'ya geçiş (NVIDIA → AMD)
- Ekran modu / konumu değişince yakalamanın yeniden kurulması
- Gecikme ölçümü (`encode` ve `tablete kadar` süreleri)

## Ölçülen (AMD entegre GPU encoder'ı ile, 120 Hz)

| | Ölçülen | Hedef |
|---|---|---|
| Yakalama → encode | 23–29 ms | ~5 ms |
| Toplam (tablette gösterime kadar) | 55–63 ms | ≤ 30 ms |
| Kare hızı | en fazla 67 fps | 120 fps |

## Denenmemiş son değişiklik

`host/crates/core/src/encode/nvenc.rs`: NVENC'i doğrudan kullanan encoder.
Yalnızca derlendi; hiçbir cihazda çalıştırılmadı. Devam edilirse ilk
denenecek şey budur.

## Bilinen sorunlar

- NVIDIA'nın Media Foundation encoder'ı test laptopunda başlamıyor
  (`ActivateObject` → `0x8000FFFF`); nedeni bulunamadı.
- Bir sürüm masaüstünü dondurdu (zorla kapatma gerekti). Olası neden:
  yakalanan karenin encode boyunca elde tutulması. Düzeltildi ve watchdog
  eklendi, ancak düzeltmenin donmayı giderdiği ayrıca doğrulanmadı.
- Driver Aygıt Yöneticisi'nden devre dışı bırakılmışsa `pnputil` onu
  etkinleştiremiyor ("Aygıt bağlı değil"); elle etkinleştirmek gerekiyor.
- HDR açıkken görüntü gidiyor (HDR Aşama 5'te planlı).
- Samsung Auto Blocker USB hata ayıklamayı kapatıyor; adb tabanlı
  bağlantıyla çakışıyor. Kalıcı çözüm AOA transport.
- Yalnızca "genişlet" modu hedeflendi; "yinele" modu tesadüfen çalıştı.

## Yapılmayanlar

Aşama 2 (touch + S Pen), Aşama 3 (ses), Aşama 4 (arayüz), Aşama 5 (HDR).
