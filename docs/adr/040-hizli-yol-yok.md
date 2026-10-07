# ADR 040 — Hızlı yol M9'da yok, ve yavaş yol ölçülecek

Status: accepted (M9 architect pass)

Kaynak: seL4 Reference Manual (v13) ve Elphinstone & Heiser,
"From L3 to seL4: What Have We Learnt in 20 Years of L4
Microkernels?" (SOSP 2013) — fastpath'in ölçülmüş etkisi orada.

## Soru

seL4'ün `fastpath`'i: belirli koşullar sağlanıyorsa IPC yolunun
büyük kısmı atlanıyor. Koşullar tipik olarak şunlar:

- mesaj dört word'den kısa (register'da sığıyor)
- capability taşınmıyor
- alıcı zaten bekliyor (randevu hemen oluyor)
- alıcının önceliği uygun
- bağlı Notification yok

Sağlanıyorsa: C/Rust yoluna hiç girmeden, elle yazılmış assembly
ile transfer ve bağlam değiştirme.

**M9'da yapacak mıyız?**

## Karar: hayır, ve bu bir erteleme değil bir sıralama

Soruyu soran bir eğilim belirtti — "önce doğru sonra hızlı, ama
yavaş yolun ölçülüp ADR'ye yazılması şartıyla" — ve **katılıyorum.**
Gerekçemi aşağıda ayrıca yazıyorum, çünkü aynı sonuca farklı
yoldan varıyorum.

### Sebep 1: fastpath bir ikinci uygulama

Hızlı yol, yavaş yolun bir optimizasyonu değil — **aynı semantiğin
ikinci bir uygulaması.** İkisi aynı şeyi yapmak zorunda, ve
"aynı"nın kontrolü testin işi.

Bu projede ADR 023 K5 şunu söylüyor: her `Err` yolu tetiklenmiş
olmalı. Fastpath bunu ikiye katlıyor — her davranışın iki yolda da
test edilmesi gerekir, ve *hangi yolun koştuğunun* teste görünür
olması gerekir.

M7'nin CR3 sayaçları bu problemi küçük ölçekte çözdü: iki sayaç,
ikisi de sıfırdan farklı olmak zorunda. Fastpath için aynısı
gerekir, ama karşılaştırılan şey bir bayrak değil **bütün IPC
semantiği**.

### Sebep 2: ölçülmemiş bir şeyi optimize etmek

M8'in syscall turu **934 çevrim** (emülasyon altında, ADR 030).
IPC'nin ne kadar süreceğini bilmiyoruz.

Fastpath'in kazancı seL4'te ~10x. Ama o kazanç, *yavaş yolun ne
kadar yavaş olduğuna* bağlı — ve bizim yavaş yolumuz henüz yok.

Önce ölç, sonra optimize et. Bu projede M5'in revoke tahmini
(56 ms) ölçülünce 6787 ms çıktı: **120 kat yanlış.** Ölçülmemiş bir
sayıya dayanarak tasarım yapmanın maliyeti kayıtlı.

### Sebep 3: doğru yol henüz yazılmadı

Fastpath'in koşulları yukarıda listelendi ve her biri bir
*değişmezi* varsayıyor. O değişmezler ADR 033-039'da yeni karara
bağlandı ve **hiçbiri çalıştırılmadı.**

Örnek: ADR 038'in bağlı Notification'ı, fastpath'in koşullarından
birini ("bağlı Notification yok") kontrol etmeyi gerektiriyor.
Ama bağlı Notification'ın kendisi henüz yazılmadı. Kontrolü
önce yazmak, kontrol edilen şeyin ne olduğunu bilmeden yazmak
demek.

### Sebep 4 (ve asıl olan): fastpath'in hatası sessiz

Yavaş yolda bir hata, bir `Err` ya da bir panic. Fastpath'te bir
hata, **yanlış koşulda alınan bir kestirme** — ve o kestirme
semantiği değiştirir, hata vermez.

Somut: fastpath "cap taşınmıyor" varsayıyor ve `caps` alanını
kontrol etmeyi unutuyor. Sonuç: cap'ler sessizce düşer, ve bu tam
olarak ADR 035'in *kasıtlı* davranışı — yani bir hata gibi
görünmez. Alıcı eksik cap alır, sebebini bulmak günler sürer.

Bu, M8'in SMEP bulgusunun tersi: orada koruma bir hatayı
*yakaladı*. Burada optimizasyon bir hatayı *gizler*.

## Ne yapılacak: yavaş yol ölçülecek

Fastpath'siz IPC'nin maliyeti M9'un kabul testinde **raporlanacak**,
ve şu üç senaryo ayrı ayrı:

| Senaryo | Neden ayrı |
|---|---|
| 4 word, cap yok, alıcı hazır | fastpath'in hedeflediği durum |
| 4 word, cap yok, alıcı hazır **değil** | bir bağlam değiştirme ekleniyor |
| 128 word, 4 cap, alıcı hazır | kopyalama + transfer maliyeti |

Birinci sayı, fastpath'in gelecekteki kazancının taban çizgisi.
Üçüncü sayı, 1 KiB kopyalamanın ne kadar tuttuğunu söylüyor — ve o
sayı ADR 034'ün "ilk dört word register'da" kararının haklı olup
olmadığını gösterecek.

Emülasyon şerhi M6'dan beri aynı ve burada da geçerli: QEMU'nun TCG
yorumlayıcısında komut maliyetleri gerçek silikonu yansıtmıyor.
Özellikle **kopyalama** yanıltıcı — QEMU'da bir `rep movsq` ile
gerçek donanımdaki aynı döngü arasındaki fark, cache davranışı
yüzünden büyük. Üçüncü senaryonun sayısı bir **üst sınır** olarak
okunmalı.

## Ne zaman yeniden bakılacak

Üç tetikleyiciden biri:

1. **M10'un zamanlayıcısı geldiğinde.** Fastpath'in koşullarından
   biri öncelik karşılaştırması; o karşılaştırma M10'da anlam
   kazanıyor.
2. **M13'ün init sistemi ölçüldüğünde.** Bir bileşen başlatma kaç
   IPC sürüyor, ve o sürenin ne kadarı IPC. Sayı küçükse fastpath
   gereksiz.
3. **Yavaş yolun ölçümü beklenenden kötü çıkarsa.** "Beklenen" ne:
   M8'in 934 çevrimlik syscall turunun iki-üç katı. Daha fazlaysa
   bir şey yanlış ve önce o bulunmalı — fastpath bir örtbas aracı
   değil.

## Fastpath yazılırsa ne gerekecek

Şimdiden yazmanın değeri, o günü kolaylaştırmak değil; **bugünkü
kararların o günü imkânsız kılmadığını doğrulamak.**

| Gereksinim | ADR 033-039 buna izin veriyor mu |
|---|---|
| Koşul kontrolü O(1) olmalı | ✓ — `caps`, `length` tag'de; alıcı durumu Endpoint'te |
| Mesaj register'da sığmalı | ✓ — ADR 034'ün ilk dört word'ü |
| Kuyruk başı O(1) okunmalı | ✓ — ADR 033'ün `queue_head` alanı |
| Badge O(1) okunmalı | ✓ — ADR 037, slot'ta |
| Bağlı Notification kontrolü O(1) | ✓ — ADR 038, TCB'de tek alan |
| Reply hedefi O(1) yazılmalı | ✓ — ADR 036, `reply_to` |

Altısı da sağlanıyor. **Fastpath'i engelleyen bir karar
alınmadı** — yalnızca ertelendi.

Bir istisna var ve yazılması gerekiyor: ADR 033'ün FIFO kuyruğu,
öncelik tabanlı bir fastpath'i zorlaştırır. seL4'ün fastpath'i
"alıcının önceliği göndereninkinden yüksek mi" diye bakar ve
değilse yavaş yola düşer. Bizim kuyruğumuzda öncelik yok, yani o
kontrol anlamsız — M10 öncelik getirdiğinde ADR 033 yeniden
okunmalı.

## Elenen alternatifler

**Fastpath'i şimdi yazmak.** Dört sebep yukarıda; en ağırı
dördüncüsü (sessiz hata).

**Fastpath'i hiç yazmamak.** Bir karar değil, bir ihmal olurdu.
Yeniden bakma tetikleyicileri yazıldı.

**Kısmi fastpath** (sadece kopyalamayı hızlandırmak, bağlam
değiştirmeyi değil). Kazancın küçük kısmı, karmaşıklığın büyük
kısmı — seL4'ün ölçümünde kazancın çoğu bağlam değiştirmeden
geliyor.

## Sonuç

- **M9'da fastpath yok.** Dört sebep: ikinci bir uygulama demek;
  ölçülmemiş bir şeyi optimize etmek; doğru yol henüz
  çalıştırılmadı; ve **fastpath hatası sessiz** — yanlış koşulda
  alınan bir kestirme semantiği değiştirir ama hata vermez.
- M5'in revoke tahmini 120 kat yanlış çıktı; ölçülmemiş sayıya
  dayanarak tasarım yapmanın maliyeti bu projede kayıtlı.
- **Yavaş yol üç senaryoda ölçülecek** ve ADR'ye yazılacak: kısa
  mesaj/alıcı hazır, kısa mesaj/alıcı hazır değil, uzun mesaj +
  cap. Üçüncüsü ADR 034'ün register kararını sınayacak.
- Emülasyon şerhi: kopyalama ölçümü QEMU'da özellikle yanıltıcı;
  **üst sınır** olarak okunmalı.
- Yeniden bakma tetikleyicileri: M10'un zamanlayıcısı, M13'ün
  ölçümü, ya da yavaş yolun beklenenin (934'ün iki-üç katı) üstüne
  çıkması.
- **Fastpath'i engelleyen bir karar alınmadı**; altı gereksinimin
  altısı da ADR 033-039 tarafından karşılanıyor. Tek not: FIFO
  kuyruğu (ADR 033) öncelik tabanlı bir fastpath'i zorlaştırır ve
  M10'da yeniden okunmalı.