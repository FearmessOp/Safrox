# ADR 054 — Konsol: font, gölge tampon ve kaydırma

- **Durum:** Kabul edildi
- **Tarih:** M15
- **Bağlam:** ADR 049 (BootInfo framebuffer), ADR 050 (device Untyped
  tohumlama), ADR 051 (PAT ve write-combining), ADR 053 (klavye)
- **Referans:** Intel SDM Vol. 3A, 12.5.2 "Precedence of Cache
  Controls" (WC bellekten okuma); VGA 8x16 metin modu font ROM düzeni

## Bağlam

Konsol, klavyeden gelen tuş olaylarını ekranda karakter olarak
göstermek zorunda. Üç şey gerekiyor: bir font, bir çizim yöntemi, ve
ekran dolduğunda kaydırma.

### screen/ bileşeni kullanılmıyor, ve bu bir karar

M14'ün `screen/` bileşeni framebuffer'ı sürebilen bir bileşen, ama
bir **servis değil**: sıfır `Recv`, tek atımlık bir betik, çizim
protokolü yok. Konsolun ondan çizim istemesi için önce ona bir
protokol eklemek gerekirdi.

Eklenmedi. Gerekçe: bir ekran sunucusu (kim çiziyor, z-sırası, girdi
yönlendirme, powerbox) katman 2'nin tasarım kararı ve kendi
architect geçişini hak ediyor. M15'in içinde aceleyle kurulan bir
protokol, sonradan atılacak bir protokol olurdu.

Konsol framebuffer device Untyped'larını root task'tan **doğrudan**
alıyor, tıpkı `screen/` gibi. İkisi aynı senaryoda birlikte
koşmuyor; M14'ün dört senaryosu olduğu gibi kalıyor ve `screen/`
kaynağına dokunulmuyor.

Gerçek pencere sunucusu geldiğinde ikisi de emekli olacak.

## Karar 1: 8x16 bitmap font, kaynakta gömülü

Font `console/src/font.rs` içinde `[[u8; 16]; 96]` olarak duruyor:
ASCII 0x20–0x7F, karakter başına 16 bayt, bayt başına bir satır, bit
7 soldaki piksel.

`include_bytes!` yerine Rust dizisi: dosya 1536 bayt ve bir `.bin`
olarak tutulsaydı derleme sırasına bir bağımlılık daha eklerdi.
Dizi olarak kaynakta durması, fontun kendisinin diff'lenebilir
olması demek.

TrueType yok. Bir rasterizer kendi başına bir milestone, ve bir
terminal için sabit genişlikli bitmap font zaten doğru araç.

**Font tablosunun kendisi testin referansı.** Kabul kriteri 4,
ekrandan okunan pikselleri bu tabloyla karşılaştırıyor; tablo hem
çizimin kaynağı hem doğrulamanın referansı olduğu için, aralarındaki
tutarlılık test edilmiyor — test edilen, çizimin tabloyu doğru
*uyguladığı*. Bu ayrım kriter 4'ün ne kanıtladığını belirliyor.

## Karar 2: karakter ızgarası RAM'de, framebuffer yalnızca yazılır

> **Düzeltme (kod yazılırken).** Bu bölümün ilk hâli 3,9 MiB'lık bir
> **piksel** gölge tamponu tarif ediyordu. Değişti; eski metin
> aşağıda duruyor ve neyin yanlış olduğu yazılı.
>
> Korunan ilke aynı: **framebuffer'dan asla okunmaz.** Değişen,
> RAM'de ne tutulduğu.

### Ne değişti

| | piksel gölge tamponu | karakter ızgarası |
|---|---|---|
| RAM | 3,9 MiB, 1000 Frame | 8 KiB, Frame yok |
| CSpace | ~2024 slot (radix 12) | ~1024 slot (radix 11) |
| kaydırmada RAM | 3,9 MiB taşınır | 8 KiB taşınır |
| kaydırmada ekrana | 3,9 MiB, **her zaman** | yalnızca **değişen hücreler** |

Son satır belirleyici. Bir konsol çoğunlukla boştur; kaydırmadan
sonra zaten boş olan bir hücre hiç piksel istemiyor. Piksel tamponu
bunu **bilemez** — elinde piksel var ve hepsi kaydı. Izgarada
"bu hücre değişmedi" tek bayt karşılaştırması.

Ayrıca gölge tampon için 1000 Frame daha retype etmek gerekirdi:
bileşene sıradan bir Untyped vermek (ADR 055'in verdiği), CSpace'i
iki katına çıkarmak, ve 1000 ek `FRAME_MAP` çağrısı. Hepsi,
ekrana her kaydırmada 3,9 MiB yazmak için.

**Neden ilk seferde görülmedi.** ADR yazılırken soru "framebuffer'ı
nasıl okumaktan kaçınırız" diye kuruldu ve gölge tampon o sorunun
doğru cevabı. Sorulmayan soru şuydu: *bu bileşen ne saklıyor?* Bir
terminal piksel saklamıyor, karakter saklıyor. Piksel, karakterin
türevi.

Kaydırma ölçümü hâlâ raporlanıyor ve yanında **yeniden boyanan
hücre sayısı** var — yani kazanç bir iddia değil, ölçüm.

### Eski metin (uygulanmadı)

Framebuffer MMIO ve write-combining ile eşli (ADR 051). WC bellekten
**okumak** patolojik: yazma tamponunun boşaltılmasını zorluyor ve
her okuma bir PCIe gidiş-dönüşü.

Konsol 1280x800x32 için 3,9 MiB'lık bir gölge tampon tutuyor (246
MiB kullanılabilir RAM'den; ADR 049'un bildirdiği geometriden
hesaplanıyor, sabit değil).

Kurallar:

- Her çizim gölge tampona yapılır.
- Gölge tampondan framebuffer'a **yalnızca değişen dikdörtgen**
  kopyalanır.
- Framebuffer'dan okuma yalnızca doğrulamada, ve yalnızca sayılı
  piksel.

### Kaydırma: gölge tamponda memmove, tek blit

Bir satır kaydırmak, naif yolla, framebuffer'dan okuyup
framebuffer'a yazmak demek — 3,9 MiB okuma + 3,9 MiB yazma, ve
okuma tarafı WC yüzünden felaket.

Seçilen yol:

1. Gölge tamponda `copy_within`: satır 1..N → satır 0..N-1.
   RAM'de, 3,9 MiB - 16 satır.
2. Son satırı gölge tamponda temizle.
3. Gölge tamponun tamamını framebuffer'a tek yönde yaz.

İkinci adımın maliyeti sıfıra yakın; asıl iş birinci ve üçüncü.
Üçüncüsü WC'nin tam da iyi olduğu şey: ardışık, tek yönlü, büyük.

**Ölçülecek, ve emülasyon şerhiyle.** ADR 051'in şerhi burada da
geçerli: QEMU'nun TCG'si WC tamponunu modellemiyor, yani ölçülen
rakam gerçek donanımın rakamı değil. Ölçüm yine de yapılıyor çünkü
iki şeyi gösteriyor: yolun çalıştığını, ve gerçek donanımda
karşılaştırılacak bir taban.

### Elenen kaydırma yöntemleri

**(d) Piksel gölge tamponu.** Yukarıda: doğru ilkeden yanlış yapı.

**(a) Framebuffer içinde memmove.** Okuma tarafı WC bellekten.
Ölçmeden reddedildi mi? Hayır — ADR 051'in ölçtüğü 117 döngü/piksel
yazma tarafı için, ve okuma en az o kadar. 1 milyon pikselin
okunması tek bir kaydırma için kabul edilemez. Yine de bu bir
tahmin, ve kaydırma ölçümü onu sınayacak biçimde raporlanıyor.

**(b) Donanım kaydırma (CRTC başlangıç adresi).** VGA metin modunda
olurdu; Limine'in doğrusal framebuffer'ında böyle bir register yok.

**(c) Halka tampon + başlangıç satırı kaydırma.** Gölge tamponu
dairesel yapıp hiç kopyalamamak, blit sırasında başlangıç satırını
kaydırmak. Daha hızlı, ve blit'i iki parçaya bölüyor. Reddedildi:
karmaşıklığı ölçülmemiş bir kazanç için. Kaydırma ölçümü gerçek
donanımda sorun gösterirse ilk bakılacak yer burası.

## Karar 3: doğrulama gölge tampondan OKUMAZ

**Bu ADR'nin en önemli maddesi.**

Gölge tampon, testin kendi kendini kandırmasının en kolay yolu:
doğrulama gölge tampondan okursa kanıtladığı tek şey "kendi RAM'ime
doğru yazdım" olur. Framebuffer'a hiç blit yapılmasa bile test
geçerdi — yani iddia kendi yokluğundan ayırt edilemez (ADR 023, K5).

Doğrulama **gerçek framebuffer'dan** okuyor, örnekleyerek.

### Kaç piksel, ve neden onlar

Tam ekran okumak (1 milyon piksel, WC bellekten) dakikalar sürerdi.
Örnekleme zorunlu. Örneklerin seçimi keyfi değil:

**Kriter 4 — basılan karakter.** Karakter başına **8 piksel**:

| Örnek | Nerede | Neyi yakalar |
|---|---|---|
| 4 "açık" | Font bitmap'inde biti 1 olan, hücrenin dört çeyreğinden birer | Karakterin çizilmediğini |
| 2 "koyu" | Biti 0 olan, glif gövdesinin içinden | Hücrenin düz doldurulduğunu |
| 2 kenar | Hücrenin sol-üst ve sağ-alt köşesinden bir piksel dışarısı | Karakterin yanlış hücreye çizildiğini |

Son iki örnek, ilk altısının yakalayamadığı hatayı yakalıyor: doğru
glif, yanlış yer. Yalnızca hücre içine bakan bir test, bir
karakterlik kayma ile başarılı olurdu.

Açık/koyu ayrımının birlikte olması gerekli: yalnızca açık piksellere
bakan bir test, hücreyi tamamen beyaza boyayan bir hatayı geçirir.

**Kriter 5 — kaydırma.** Kaydırmadan önce ve sonra, **üst satırdan 3
ve alt satırdan 3** piksel. Önce: üstte 'A' gliflerinin, altta boşluk.
Sonra: üstte ikinci satırın glifleri, altta yeni içerik. Altı piksel
ikisini birden gösteriyor — içeriğin yukarı taşındığını ve altın
temizlendiğini.

Kaydırmanın "çalıştığını" yalnızca üstten okuyarak iddia etmek,
alt satırı temizlemeyen bir kaydırmayı geçirirdi.

Toplam okuma: karakter başına 8, beş karakter için 40; kaydırma için
6. Elli civarı piksel — WC bellekten bile ihmal edilebilir.

## Karar 4: imleç ve satır düzenleme

İmleç, mevcut hücrenin alt iki satırını dolduran bir blok. Yanıp
sönme yok: bir zamanlayıcı gerektirirdi ve ölçtüğü hiçbir şey yok.

Satır düzenleme asgari: yazdırılabilir karakter ekler, Backspace
siler, Enter satır sonuna geçer. Sol/sağ ok tuşları imleci hareket
ettiriyor — 0xE0 uzatmasının gözlemlenebilir olması için (kriter 3),
yoksa uzatılmış tuşların çözüldüğü yalnızca sürücünün kendi
raporuna dayanırdı.

Geçmiş yok, sekme yok, ANSI escape yok. Hiçbiri M15'in kriterlerinde
geçmiyor.

## Sonuçlar

- 8x16 bitmap font, kaynakta dizi olarak; hem çizimin kaynağı hem
  doğrulamanın referansı.
- Gölge tampon RAM'de; framebuffer yalnızca yazılıyor.
- Kaydırma: gölge tamponda `copy_within` + tek blit. Ölçülüyor,
  emülasyon şerhiyle.
- **Doğrulama gerçek framebuffer'dan örnekliyor**: karakter başına 8
  piksel (4 açık, 2 koyu, 2 kenar dışı), kaydırma için 6.
- Konsol framebuffer'ı kendi eşliyor; `screen/` değişmiyor,
  kullanılmıyor.
- Kaydırma ölçümü ve WC okuma maliyeti `hardware-todo.md` madde 12.