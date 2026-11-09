# Sanal ekran driver'ı

TabLink Display kendi driver'ını içermez; açık kaynak
[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
(IddCx) üzerine kurulur.

## Doğrulananlar (2026-11-09, projenin README, wiki ve LICENSE sayfalarından)

| Konu | Bulgu |
|---|---|
| Lisans | MIT; telif bildirimi korunarak yeniden dağıtılabilir |
| İmza | SignPath Foundation sertifikasıyla imzalı; test-signing yalnızca ARM64 için anılıyor |
| Yapılandırma | `C:\VirtualDisplayDriver\vdd_settings.xml` |
| Çözünürlük / yenileme hızı | `<resolutions><resolution>` ve `<global><g_refresh_rate>` |
| GPU seçimi | `<gpu><friendlyname>`; Device Manager'daki ad veya `default` |
| HDR | Windows 11 23H2+; `<colour>` altında `HDRPlus` / `SDR10bit` |
| Yeniden yükleme | Aygıtı devre dışı bırakıp etkinleştirmek |

## Doğrulanamayanlar

- Driver'ın hardware ID'si ve Device Manager'daki tam adı. Host bu yüzden
  aygıtı ada göre arar (`Virtual Display`, `IddSampleDriver`, `MttVDD`).
- Komut satırından kurulum. Kurulum bu yüzden elle yapılır (aşağıda).
- Named pipe komutları belgelenmemiş; kullanılmıyor.
- En güncel release'in tarihi ve dosya adları.

Bu varsayımlar yalnızca gerçek cihazda sınanabilir.

## Kurulum (bir kereliğine)

Güncel ve yetkili adımlar projenin kendi
[Installation](https://github.com/VirtualDrivers/Virtual-Display-Driver/wiki/Installation)
sayfasındadır. Özet:

1. Projenin Releases sayfasından son sürümü indirin.
2. Kurulum programı varsa onu çalıştırın. Yoksa klasörü
   `C:\VirtualDisplayDriver\` konumuna kopyalayın, ardından
   Device Manager → Action → Add legacy hardware → listeden seç (Advanced) →
   Display adapters → Have Disk ile `.inf` dosyasını gösterin.
   `.inf` dosyasına sağ tıklayıp doğrudan kurmayın.
3. Device Manager → Display adapters altında "Virtual Display Driver"
   göründüğünü doğrulayın.

Ekran kartı veya chipset driver'ını büyük sürüm güncellemeden önce bu driver'ı
kaldırmanız önerilir (projenin kendi uyarısı).

## Host ne yapar

- `vdd_settings.xml` içinde yalnızca `monitors`, `global`, `resolutions` ve
  (NVIDIA adaptör varsa) `gpu` bölümlerini tabletin bildirdiği değerlerle
  değiştirir; diğer bölümlere dokunmaz.
- Aygıtı `pnputil` ile etkinleştirir, ayar değiştiyse yeniden başlatır,
  bağlantı kesilince devre dışı bırakır. Bu işlemler **yönetici yetkisi**
  gerektirir; host'u "Run as administrator" ile çalıştırın.
