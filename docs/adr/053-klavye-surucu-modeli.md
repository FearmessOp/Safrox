# ADR 053 — Klavye sürücü modeli ve scan code seti

- **Durum:** Kabul edildi
- **Tarih:** M15
- **Bağlam:** ADR 042 (IRQ capability'si), ADR 045 (I/O port erişim
  modeli), ADR 046 (sürücü bileşeni deseni)
- **Referans:** IBM PS/2 Technical Reference, "Keyboard and Auxiliary
  Device Controller"; Intel 8042 veri sayfası; QEMU `hw/input/pckbd.c`
  (translation biti), `hw/input/ps2.c`

## Bağlam

M13 seri sürücüyü CPL 3'e taşıdı: IOPort capability'si + IRQHandler
capability'si + Endpoint. M15 aynı deseni ikinci bir cihaz için
tekrarlıyor. Kernel'e yeni bir mekanizma eklenmiyor; bu ADR'nin
kanıtlaması gereken şey desenin **genel** olduğu, seri porta özel
olmadığı.

Klavye bunu sınamak için seri porttan daha iyi bir cihaz: iki ayrı
port (0x60 veri, 0x64 durum/komut), durum makinesi olan bir protokol
(prefix baytları, make/break), ve çıktısı başka bir bileşene giden
bir veri akışı.

## Olgu: scan code seti 2 değil, 1

Brief "scan code set 2" diyordu. **Ölçüldü ve yanlış.**

`scripts/m15-spike/` altındaki 512 baytlık boot sector 0x64'ün
durum register'ını yoklayıp 0x60'tan okuduğu her baytı seri porta
onaltılık basıyor. Projenin kullandığı QEMU bayraklarıyla
(`-M q35 -display none`), QMP `send-key` ile beş tuş gönderildiğinde:

```
SPIKE-READY 1e 9e 2a 1e 9e aa e0 48 e0 c8 1d 2e ae 9d 38 2d ad b8
```

Çözümü:

| Tuş | Baytlar | Okunuşu |
|---|---|---|
| `a` | `1e 9e` | make 0x1e, break = make \| 0x80 |
| `shift+a` | `2a 1e 9e aa` | iç içe; değiştirici en dışta |
| `up` | `e0 48 e0 c8` | **0xE0 çeviriden sağ çıkıyor**, break'te tekrarlıyor |
| `ctrl+c` | `1d 2e ae 9d` | iç içe |
| `alt+x` | `38 2d ad b8` | iç içe |

Set 2'de `a` 0x1C olur ve break iki bayttır (`F0 1C`). Gelen
`1e`/`9e`, **set 1**'dir: i8042'nin çeviri (translation) biti açık ve
onu açan SeaBIOS, POST sırasında.

### Bu, brief'in iki maddesinin çakışması

Brief hem "set 2" hem "PS/2 denetleyici ilklendirmesi kapsam dışı"
diyordu. İkisi birden olamaz: set 2'yi görmek için denetleyici
yapılandırma baytının bit 6'sını (translation) temizlemek gerekir, ki
bu 0x64'e 0x20 (read config) yazıp 0x60'tan okumak, biti temizleyip
0x64'e 0x60 (write config) ile geri yazmaktır — tanımı gereği
ilklendirme.

**Daha güçlü kısıt olan "ilklendirme yok" seçildi.** Sürücü, gerçekten
gelen şeye göre yazılıyor: set 1.

Kabul kriterlerinin hiçbiri set numarasına bağlı değil — dönüşüm,
make/break ayrımı, değiştiriciler ve 0xE0 uzatması her iki sette de
var, yalnızca sayılar farklı. Kriterler ölçtükleri şeyi ölçmeye devam
ediyor.

### Taşınabilirlik borcu, açıkça

Set 1'e bağlanmak, "BIOS çeviriyi açık bıraktı" varsayımına
bağlanmaktır. UEFI ile boot eden bir makinede, ya da USB klavyeyi
i8042 üzerinden taklit etmeyen bir makinede bu varsayım tutmayabilir.

Sürücü bunu **sessizce** kabul etmiyor: tanımadığı bir bayt gördüğünde
sayacını artırıyor ve raporluyor. Tamamı tanınmayan bir akış, sıfır
tuş üreten bir sürücüden farklı görünüyor.

`docs/hardware-todo.md` madde 11.

## Karar 1: iki ayrı IOPort capability'si

Klavye 0x60 ve 0x64 kullanıyor. Tek bir `[0x60, 0x64]` aralığı
vermek en kolay yol, ve gereğinden geniş: arada 0x61 var (PC
hoparlörü ve NMI durum register'ı) ve 0x62/0x63 (eski denetleyici
register'ları).

`MAX_RANGES = 16` ve şu an iki aralık kullanımda. **İki ayrı
capability bedava**: `[0x60, 0x60]` ve `[0x64, 0x64]`.

Bedava olmayan bir şey de kazandırıyor: `kbd-no-cap` negatifi artık
*hangi* capability'nin eksik olduğunu ayırt edebiliyor. Tek aralık
olsaydı "klavye yetkisi yok" tek bir durumdu; ikiyle, veri portu olup
durum portu olmayan bir sürücünün ne yaptığı da bir senaryo.

ADR 045'in 0x64'ü "makineyi resetleyebilir" diye anması bir yasak
değildi — IOPL'nin *kasıtsız* olarak neyi açacağına örnekti. Burada
0x64 kasıtlı ve dar veriliyor.

## Karar 2: durum makinesi, tablo değil

Scan code → tuş dönüşümü iki parçaya ayrılıyor:

1. **Byte çözücü** (`Decoder`): bayt akışından olay üretir. Durumu
   tek bir `pending_e0: bool`. 0xE0 gördüğünde bayrağı kaldırır ve
   olay üretmez; sonraki bayt uzatılmış tuş olur. Bit 7 break
   demektir.
2. **Düzen** (`Layout`): tuş kodu + değiştirici durumu → karakter.
   Değiştirici durumu çözücüde değil burada tutulur, çünkü bir
   değiştiricinin basılı olması bir *olay* değil bir *durum*.

Ayrımın sebebi test edilebilirlik: çözücü saf bir fonksiyon, girdisi
bayt dizisi çıktısı olay dizisi, ve donanımsız sınanabiliyor. Tek bir
`match` içinde birleştirilseydi `kbd-test` ancak gerçek bir IRQ
geldiğinde bir şey söyleyebilirdi.

**Tanınmayan bayt bir hata değil, bir olay.** Sürücü onu atıp
sayacını artırıyor. Alternatif — panic — bir klavye arızasını sistem
arızasına çevirirdi.

## Karar 3: IRQ 1, kesme ile; yoklama değil

Seri sürücü deseni: IRQHandler capability'si bir Notification'a
bağlanıyor, sürücü thread'i `Wait` ile uyuyor, kesme geldiğinde
uyanıyor, porttan okuyor, `Ack` ile hattı açıyor.

Yoklama (polling) bu bileşen için daha basit olurdu ve **ölçümü
anlamsız kılardı**: tuş→piksel gecikmesinin anlamı, kesmeden
başlayan bir zincirin ölçülmesi. Yoklayan bir sürücüde ölçülen şey
yoklama aralığı olurdu.

`MAX_IRQ = 16` ve IRQ 1 için kernel'de özel bir şey yok; `irq.rs`
hat numarasına göre ayrım yapmıyor. Desenin genel olduğu iddiası
burada sınanıyor: M13'ün kodu IRQ 4 için yazılmıştı ve IRQ 1 için
değişmesi gerekmedi.

### Çıkış tamponu boşalana kadar okumak

i8042'nin bir inceliği: bir kesme birden fazla bayt anlamına
gelebilir, ve bayt okunmadan yeni kesme gelmez. Sürücü uyandığında
**durum register'ının bit 0'ı temizlenene kadar** okuyor, tek bayt
değil.

Tek bayt okunsaydı `shift+a` (dört bayt) dört kesme gerektirirdi ve
üçü hiç gelmezdi — tampon dolu kaldığı için. Belirtisi: ilk tuştan
sonra klavyenin tamamen susması.

Döngü sınırsız değil (ADR 023, "kernel yolunda sınırsız döngü yok"):
en çok 16 bayt okuyup çıkıyor. 16, en uzun bilinen diziden (Pause,
6 bayt) fazla; aşılması bir donanım arızası demektir ve sayılıyor.

## Karar 4: sınırlı tampon, en eskiyi düşür

Sürücü ile konsol arasında Endpoint var ve konsol her zaman almaya
hazır değil (çiziyor olabilir, kaydırıyor olabilir). Kesme ise
beklemiyor.

Sürücü, üretilen tuş olaylarını **32 girdilik bir halka tamponda**
tutuyor. Dolduğunda: **en eski düşer**.

| Seçenek | Davranış | Neden değil |
|---|---|---|
| Sınırsız büyüme | — | Yasak. Bellek tükenene kadar bir cihazın hızına bağlı. |
| Yeniyi reddet | Son tuşlar kaybolur | Kullanıcı en son bastığını kaybeder; en şaşırtıcı davranış. |
| **En eskiyi düşür** | İlk tuşlar kaybolur | Seçildi. |

Gerekçe: bir terminalde geç kalmış girdi zaten değerini yitirmiş
oluyor, ve kullanıcının en son yazdığının görünmesi en az şaşırtan
davranış. Ayrıca `kbd-flood` bunu gözlemlenebilir kılıyor —
düşürülen her olay sayılıyor ve seri porta raporlanıyor, yani
"tampon taştı" sessiz bir olay değil.

32 sayısı: bir insanın saniyede ~10 tuşu ve konsolun en yavaş
işleminin (kaydırma) süresi birlikte, iki mertebe pay bırakıyor.
Ölçüm sonrası gözden geçirilecek bir sayı, ve `kbd-flood` onu
doğrudan sınıyor.

## Elenen alternatifler

**(a) Klavyeyi konsolun içine koymak.** Tek bileşen, Endpoint yok,
daha az kod. Reddedildi: M15'in asıl iddiası iki bileşenin
yalıtılmış olduğu ve yalnızca Endpoint üzerinden konuştuğu. Tek
bileşende bu iddia test edilemez hale gelirdi — kendi yokluğundan
ayırt edilemeyen bir iddia (ADR 023, K5).

**(b) Denetleyiciyi ilklendirip set 2'ye geçmek.** Brief'in
istediği, ve kapsam dışı bırakılan şeyin ta kendisi. Reset komutu
(0xFF), yanıt bekleme (0xFA/0xAA), zaman aşımı yönetimi, ve
başarısızlıkta ne yapılacağı — hepsi ayrı kararlar. Kazancı
taşınabilirlik, bedeli M15'in iki katı. Ertelendi, borç kaydedildi.

**(c) Tuş olaylarını bayt olarak göndermek, karakteri konsolda
çözmek.** Sürücü ham scan code'u iletirdi. Reddedildi: düzen bilgisi
(hangi tuş hangi karakter) klavyeye ait, konsola değil. Türkçe bir
düzen geldiğinde değişmesi gereken yer sürücü olmalı.

## Sonuçlar

- Sürücü set 1'e göre yazıldı; ölçüldüğü için biliniyor,
  varsayıldığı için değil.
- İki IOPort capability'si: `[0x60,0x60]` ve `[0x64,0x64]`.
- Çözücü saf ve ayrı test edilebilir; düzen ondan ayrı.
- Kesme ile çalışıyor, uyandığında tamponu boşaltıyor, en çok 16
  bayt.
- 32 girdilik halka tampon, taşınca en eski düşüyor, düşen sayılıyor.
- Tanınmayan baytlar sayılıyor ve raporlanıyor.
- PS/2 ilklendirmesi yok; set 1 bağımlılığı `hardware-todo.md`
  madde 11.