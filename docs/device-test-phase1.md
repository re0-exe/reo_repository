# Aşama 1 — Cihaz testi

Bu aşamada beklenen: tablet, Windows'ta ikinci monitör olarak **görüntü** verir.
Dokunma, S Pen, ses ve ayar arayüzü henüz yok.

> Bu sürüm gerçek donanımda hiç çalıştırılmadı. İlk denemede sorun çıkması
> normaldir; her adımın altındaki "sorun olursa" notuna göre çıktıyı gönderin.

## 1. Dosyaları indirin

GitHub → repo → **Actions** → en üstteki yeşil `build` çalışması → sayfa altı
**Artifacts**:

- `tablink-host` → `tablink-host.exe`
- `tablink-android` → `app-debug.apk`

## 2. adb'yi hazırlayın

[Android platform-tools](https://developer.android.com/tools/releases/platform-tools)
paketini indirip açın. İçindeki `adb.exe`, `AdbWinApi.dll` ve
`AdbWinUsbApi.dll` dosyalarını `tablink-host.exe` ile aynı klasöre kopyalayın.

## 3. Sanal ekran driver'ını kurun (bir kez)

[`docs/driver.md`](driver.md) içindeki adımları izleyin.

## 4. Tableti hazırlayın (bir kez)

1. Settings → About tablet → Software information → **Build number**'a 7 kez
   dokunun (Developer options açılır).
2. Settings → Developer options → **USB debugging**'i açın.
3. Tableti USB ile bilgisayara bağlayın, tablette çıkan izin penceresinde
   "Always allow from this computer"ı işaretleyip onaylayın.
4. Uygulamayı kurun (klasörde bir terminal açıp):
   ```
   adb install -r app-debug.apk
   ```

## 5. Çalıştırın

`tablink-host.exe` dosyasına sağ tıklayın → **Run as administrator**.
(Sanal monitörü açıp kapatmak yönetici yetkisi ister.)

Tablette uygulama kendiliğinden açılmalı ve birkaç saniye içinde Windows
masaüstünün uzantısı görünmelidir. 60 Hz denemek için:
`tablink-host.exe --fps 60`

## 6. Kontrol listesi

| # | Kontrol | Beklenen |
|---|---|---|
| 1 | Settings → System → Display | İkinci bir monitör görünür |
| 2 | O monitörün çözünürlüğü | 2800 × 1752 |
| 3 | Advanced display → refresh rate | 60 ve 120 Hz seçilebilir |
| 4 | "Extend" ve "Duplicate" | İkisi de tablette doğru görünür |
| 5 | Bir pencereyi tablete sürükleyin | Akıcı, metin keskin |
| 6 | Konsoldaki satır | `… fps \| USB ping … ms \| atlanan frame …` |
| 7 | Kabloyu çıkarıp takın | Birkaç saniyede yeniden bağlanır |
| 8 | Windows'ta yenileme hızını veya yönü değiştirin | Görüntü kısa bir kesintiyle geri gelir |
| 9 | Tablette başka uygulamaya geçip geri dönün | Görüntü geri gelir |
| 10 | Fareyi tablet ekranına götürün | İmleç görünür ve akıcı hareket eder |
| 11 | Bilgisayarı kilitleyip (Win+L) açın | Yayın kendiliğinden devam eder |
| 12 | Konsolda Ctrl+C | Program kapanır, ikinci monitör Windows'tan kalkar |
| 13 | Host'u yönetici olmadan çalıştırıp kapatın, sonra yönetici olarak açın | Tablet yine bağlanır |

## Sorun olursa bana gönderin

- Konsol penceresindeki **tüm metin**.
- Kontrol listesinde hangi numaranın başarısız olduğu ve ekranda ne gördüğünüz.
- Tablet tarafı için (isteğe bağlı): `adb logcat -d -s TabLinkStream TabLinkDecoder`

## Bilinen sınırlar

- "USB ping" yalnızca bağlantının gidiş-dönüş süresidir; ekrandan ekrana
  toplam gecikme değildir. Gerçek ölçüm aracı Aşama 4'te gelecek.
- Windows'ta ekran yönünü "Portrait" yaparsanız görüntü tablette yan durur;
  tableti fiziksel olarak çevirmeniz gerekir.
- 256 pikselden büyük özel imleçler kırpılır.
- DRM korumalı içerik (Netflix vb.) siyah görünür.
- Driver, laptopunuzun HDR'ı açıkken farklı davranabilir; Aşama 1'de HDR'ı
  kapalı tutun.
