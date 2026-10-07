# ADR 027 — VSpace: user adres uzayı bir capability

Status: accepted (M7)

M3 kernel'e kendi sayfa tablolarını verdi. M5 belleği capability'lere
bağladı. M7 ikisini birleştiriyor: bir thread'in adres uzayı artık bir
**PageTable capability'si**, ve içine ne eşleneceğine capability'leri
tutan karar veriyor — kernel değil.

Ring 3 **yok**. Kod hâlâ CPL 0'da çalışıyor, ama kendi CR3'ünde. Bu
bölünme kasıtlı ve ADR'nin ilk kararı.

## Neden ayrıcalık seviyesi ve adres uzayı ayrı milestone'lar

İkisini birlikte yapmak cazip: "user thread" dendiğinde akla gelen şey
zaten ikisinin birleşimi. Ama birlikte yapılırsa ilk `#GP` veya `#PF`
geldiğinde elde iki şüpheli olur ve hangisinin bozuk olduğu ayırt
edilemez:

- CR3 yanlış mı kuruldu, yoksa
- `iretq` ile ring 3'e geçiş mi yanlış?

İkisi de "sayfa hatası" ya da "genel koruma hatası" olarak görünür.
M3'ün `verify_before_switch`'i tam da bu yüzden var: CR3 değişiminin
bir sonraki komutta sessiz bir triple fault'a dönüşmesi, bu projenin
en pahalı hata sınıfı.

**M7 adres uzayını doğruluyor, M8 ayrıcalık seviyesini.** M7 bittiğinde
"iki thread aynı sanal adreste farklı veri görüyor" kanıtlanmış olacak;
M8 bunun üstüne yalnızca CPL'i ekleyecek.

## VSpace nedir

Bir **kök PageTable capability'si**. Nesne 4 KiB (ADR 017'de sabit), ve
bir PML4 tutuyor. TCB bir VSpace'e bağlanıyor (`SetSpace`), ve bağlam
değiştirme CR3'ü ona göre yüklüyor.

Ayrı bir `VSpace` nesne tipi **eklenmedi**. ADR 017 sekiz tipi
sabitledi ve "M13'ten önce değişmeyecek" dedi; kök PML4 zaten bir
PageTable, ve onu ayrı bir tip yapmak dokuzuncu tipi getirirdi. Kökü
diğerlerinden ayıran tek şey bir TCB'ye bağlı olması, ve bu TCB'de
kayıtlı — nesnede değil.

## Kernel yarısı: her VSpace'te aynı, ve GLOBAL

Bir user VSpace'i yaratılırken kernel'in PML4 girdileri (indeks
256..512, yani üst yarı) kopyalanıyor. Üç sonucu var:

**Kernel her adres uzayında görünür.** Kesme geldiğinde hangi CR3'te
olursak olalım handler, IDT, kernel stack ve PMAP erişilebilir. Bu
olmadan bir kesme, CR3 değiştirmiş bir thread'de triple fault'tur.

**Girdiler GLOBAL.** M3 kernel eşlemelerini `PageFlags::GLOBAL` ile
kurdu ve o kararın bedeli M7'de ödeniyor, faydası da: CR3 yazmak
global olmayan TLB girdilerini atar, kernel'inkileri bırakır. Adres
uzayı değiştirmenin maliyeti bu yüzden tam bir TLB boşaltması değil.

**PML4 girdileri *paylaşılıyor*, kopyalanmıyor.** Kopyalanan şey PML4
seviyesindeki 8 baytlık girdi; altındaki PDPT/PD/PT tabloları tek bir
fiziksel kopya olarak kalıyor ve tüm VSpace'ler aynı tablolara bakıyor.
Bu bir optimizasyon değil, zorunluluk: kernel bir eşlemeyi
değiştirdiğinde (heap büyüdüğünde) her VSpace'in görmesi gerekiyor.

Ve tehlikenin kaynağı da bu. Bir user VSpace'inde kernel yarısına bir
sayfa eşlemeye çalışmak, **paylaşılan** bir ara tabloyu değiştirirdi —
yani tek bir user bileşeni, her adres uzayındaki kernel eşlemelerini
bozabilirdi. Sonucu bir izolasyon ihlali değil, doğrudan kernel
bütünlüğünün kaybı.

### Değişmez K1: bir VSpace'e yalnızca user yarısına eşleme yapılabilir

Capability yolundaki her `Map`, hedef adresin `layout::is_user()`
olduğunu doğruluyor. M3'ün `paging::check_address`'i zaten `WrongHalf`
döndürüyor, ama o kontrol `PageFlags::USER` bayrağına bakıyor — yani
"user bayrağı var ama kernel adresi" durumunu yakalıyor. Capability
katmanı daha katı: **hedef VSpace bir user VSpace'i ise, adres user
yarısında olmak zorunda**, bayraklardan bağımsız.

İki kontrol üst üste, ve ikisi de test ediliyor. Birinin diğerini
gereksiz kıldığı doğru değil: M3'ünki bayrak tutarlılığını, M7'ninki
nesne sahipliğini koruyor.

#### "İkisi de test ediliyor" — önce doğru değildi

Yukarıdaki cümle ilk yazıldığında bir iddiaydı, ve yanlıştı. Mutasyon
testi gösterdi: K1'i silip `vspace-kernel-half` senaryosunu koşmak
**geçti.**

Sebep, iki kontrolün aynı hatayı üretmesiydi. `paging::map` aynı dört
adresi `WrongHalf` ile reddediyordu, capability katmanı onu
`NotUserAddress`'e çeviriyordu, ve test "reddedildi" görüp memnun
oluyordu. Hangi guard'ın ateşlendiği loga hiç yansımıyordu.

M5'in `SlotNotEmpty` bulgusuyla tıpatıp aynı şekil: bir kontrol, kendi
yokluğundan ayırt edilemiyor.

**Düzeltme:** iki ayrı hata değeri. `NotUserAddress` capability
katmanının sahiplik reddi; `FlagsWrongHalf` paging katmanının tutarlılık
reddi. Test ikisini de görüyor, ve K1 silindiğinde artık ilk adreste
düşüyor.

İkinci hatayı *tetiklemek* de iki deneme aldı, ve ikisi de öğretici:

- `KERNEL_MIN + 0x2000` PMAP'in içinde, zaten eşli → `AlreadyMapped`
  yarı kontrolünden önce dönüyor.
- USER bayrağı **olmayan** bir kernel adresi tutarlı: ikisi de "kernel"
  diyor, çelişki yok.

Tetikleyen şey, kernel adresinde **USER bayraklı** bir eşleme: bayraklar
"user" diyor, adres "kernel" diyor. Bir hatayı tetiklemeyi denemek,
hatanın ne olduğunu gerçekten anlamayı gerektiriyor — bu yüzden K5 bir
formalite değil.

### Değişmez K2: yeni bir kernel PML4 girdisi ortaya çıkamaz

Kopyalama bir anlık görüntü. Kernel boot'tan *sonra* yeni bir PML4
girdisi yaratırsa — yani daha önce hiç dokunulmamış bir 512 GiB
bölgeye ilk eşlemesini yaparsa — o girdi önceden yaratılmış
VSpace'lerde **olmaz**, ve o VSpace'te çalışan bir thread kernel'in bir
parçasını göremez.

Bu gerçek bir risk, çünkü heap talep üzerine büyüyor. Kurtaran şey
KHEAP'in tek bir PML4 girdisine (384) sığması: 64 MiB limit, 512
GiB'lik bir girdinin içinde. Yani girdi boot'ta zaten var ve heap
büyüdükçe yalnızca altındaki tablolar doluyor — paylaşılan tablolar.

Varsayıma güvenmek yerine ölçülüyor: boot'ta kernel PML4'ünün hangi
üst-yarı girdilerinin dolu olduğu kaydediliyor, ve her VSpace
yaratımında aynı küme olup olmadığı kontrol ediliyor. Değişmişse
`panic` — çünkü bu, sessizce yanlış çalışacak bir durumun tek erken
belirtisi.

## Ara tablolar açıkça sağlanıyor

Bir Frame'i 0x400000'a eşlemek için PDPT, PD ve PT gerekiyor. Bunlar
nereden geliyor?

**seL4 modeli: çağıran sağlar.** `Map` bir PageTable capability'si
alıyor ve onu belirtilen seviyeye yerleştiriyor. Eksik tablo bir hata,
kernel'in sessizce çözdüğü bir şey değil.

Alternatif — kernel'in bir Untyped'dan otomatik ayırması — testi
kısaltırdı ve yanlış olurdu. İki sebep:

1. **Bütçe görünür kalmalı.** ADR 022'nin kuralı: kernel kendi adına
   bellek ayırmaz. Otomatik ayırma, bir `Map` çağrısının kaç frame
   tükettiğini çağıranın göremediği bir şeye dönüştürürdü.
2. **Sınırsız döngü yok.** Otomatik ayırma, "yeterli tablo var mı"
   sorusunu çağrı sırasında cevaplamayı gerektirir; açık model bunu
   çağırana taşıyor.

Uygulama tarafında bunun hoş bir sonucu var: M3'ün `paging::map`'i
değişmeden kullanılıyor. Ona **hiçbir zaman frame vermeyen** bir
`FrameSource` geçiliyor; eksik bir ara tablo `MapError::OutOfFrames`
olarak dönüyor ve capability katmanı bunu "önce bu seviyeye bir
PageTable eşle" anlamına gelen bir hataya çeviriyor.

```rust
struct NoFrames;
impl FrameSource for NoFrames {
    fn alloc_frame(&mut self) -> Option<PhysFrame> { None }
}
```

M3'ün API'sine tek satır dokunulmadı. ADR 011'in "mekanizma, politika
değil" iddiası burada sınandı ve geçti.

## Haklar eşleme izinlerine nasıl çevriliyor

ADR 021'in dört biti ile sayfa bayrakları aynı şey değil. Çeviri:

| Cap hakkı | Eşlemeye etkisi |
|---|---|
| `READ` yok | Eşlenemez — `InsufficientRights` |
| `READ` var | `PageFlags::READ` |
| `WRITE` var | `PageFlags::WRITE` eklenir |
| `WRITE` yok | Salt okunur eşlenir |
| `GRANT` | **Eşleme ile ilgisiz** |
| `GRANTREPLY` | **Eşleme ile ilgisiz** |

`GRANT`'ın ilgisiz olması kayda değer, çünkü ilk bakışta "bu Frame'i
başkasına verebilir miyim" gibi okunuyor. Değil: `GRANT` bir
*Endpoint* üzerinden capability taşıma hakkı (ADR 021). Bir Frame'i
başkasına vermek `copy` ile olur ve onun kuralları ayrı.

`EXEC` **verilmiyor**. M7'de eşlenen her sayfa NX. Çalıştırılabilir
user sayfaları M8'in işi, ve W^X'in user tarafında ne anlama geldiği
orada karara bağlanacak. Şimdilik vermemek, sonra vermekten kolay.

### SMAP: M8 sanılan borç M7'de geldi

Bilinen eksikler listesi `stac`/`clac` için "user eşlemeleri var
olduğunda gerekecek — **M8**" diyordu. Bir milestone geç yazılmış, ve
sebebi öğrenilmeye değer.

**SMAP, sayfanın USER bitine bakar; kodun ayrıcalık seviyesine
değil.** M7 USER işaretli sayfalar yaratıyor — bir user adres uzayının
tanımı bu — ama her thread hâlâ CPL 0'da. Yani ilk user eşlemesi ile
ilk SMAP ihlali aynı anda geliyor, ilk ring 3 komutundan bir milestone
önce.

Kabul testi bunu ilk koşuda buldu: `supervisor read
protection-violation`, açıkça mevcut ve açıkça okunabilir bir sayfada.

Çözüm `cpu::UserAccess`, bir RAII guard. Elle yazılmış `stac`...`clac`
çiftlerinin tek bir hata modu var ve sessiz: aradaki erken bir
`return` AC'yi set bırakır, ve o noktadan sonra SMAP hiçbir şey
korumaz. Hiçbir şey hata vermez, hiçbir şey loglanmaz. `Drop` her
çıkış yolunda çalışır — ADR 012'nin `TlbFlush` için verdiği aynı karar.

İç içe guard'lar dıştakinin iznini iptal etmiyor; ADR 025'in
`InterruptGuard` için koyduğu kuralın aynısı, ve testi de aynı şekilde
yazıldı.

## CR3 yalnızca gerektiğinde değişiyor

Bağlam değiştirme iki thread arasında. Eğer ikisi aynı VSpace'i
paylaşıyorsa — M7'de bu yaygın, çünkü çoğu thread hâlâ kernel
VSpace'inde — CR3'e dokunmak saf kayıp: global olmayan tüm TLB
girdileri atılır ve hiçbir şey değişmez.

```rust
if next_root != current_root {
    paging::load_table(next_root);
}
```

İki sayaç basılıyor: değişen ve atlanan. Testin bunu *ikisini birden*
görmesi gerekiyor — yalnızca "CR3 değişti" demek, her switch'te
değiştiren bir uygulamayı da geçirirdi.

Maliyeti ölçülüyor (aşağıda). M6'nın switch başına 328 çevrimi taban
çizgisi.

## Ölçüm: TLB boşaltmasının bedeli

`vspace-test` iki ölçüm yapıyor: aynı VSpace'li iki thread arasında
10 000 switch, ve farklı VSpace'li iki thread arasında 10 000 switch.
Fark, CR3 yazmanın ve ardından gelen TLB ıskalarının bedeli.

Emülasyon şerhi M6'dakiyle aynı ve burada daha da önemli: QEMU'nun
TLB'si gerçek silikon değil, ve TCG yorumlayıcısında bir TLB ıskası
gerçek donanımdaki gibi davranmıyor. Çıkan sayı bir büyüklük
mertebesi; **gerçek donanımda farkın daha büyük olması beklenir**,
çünkü asıl maliyet CR3 yazmanın kendisi değil, sonrasındaki soğuk
TLB.

## Uygulama düzeltmesi: "bir VSpace'i revoke etmek" diye bir şey yok

Bu ADR'nin ilk hali, bir adres uzayını yıkmanın kök PageTable
capability'sini revoke etmek olduğunu varsayıyordu. Kod öyle yazıldı,
koştu, ve **hiçbir şey silmedi** — `0 capabilities deleted`, watermark
yerinde.

Sebep, derivation ağacının ne olduğuyla ilgili. Revoke bir
capability'nin **soyundan gelenleri** siler (ADR 019). Kök, üç ara
tablo ve Frame — hepsi *aynı Untyped'ın* çocukları. Birbirlerinin
kardeşi, zinciri değil. Kök, diğerlerinin atası olmadığı için onlara
ulaşamıyor.

Daha derin olan şey şu: **"bir VSpace" tek bir nesne değil, bir
şekil.** Capability'lerden kurulmuş bir yapı, ve o yapıyı kaydeden
hiçbir şey yok — derivation ağacı "bu tablo şu kökün altında" bilgisini
tutmuyor, çünkü tutması gerekmiyor. Hepsine birden sahip olan tek şey
Untyped.

**Düzeltme:** `revoke_vspace` Untyped slot'unu alıyor, kök slot'unu
yalnızca "bu CR3'te mi" kontrolü için. Bellek ADR 020'nin dediği gibi
geri geliyor: watermark sıfırlanarak, nesne nesne değil.

Bu, ADR 020'nin kararının beklenmedik bir doğrulaması. "Untyped bir
ayırıcı değil, revoke bir watermark sıfırlamasıdır" demek, yan etki
olarak "bir adres uzayını yıkmak tek bir işlemdir" sonucunu veriyor.
Nesne nesne serbest bırakan bir tasarımda VSpace'in şeklini bir yerde
tutmak gerekirdi.

## ADR 015'in borcu

ADR 015 `unmap`'in ara sayfa tablolarını serbest bırakmadığını kabul
etmiş ve "M5, capability nesne modeliyle" demişti. M5 geldi ve
bırakmadı; borç M7'ye devroldu.

**M7 borcu kapatıyor, ama ödeyerek değil — devrederek.** Ara tablolar
artık PageTable capability'leri. `Unmap` tabloyu ağaçtan çıkarıyor ve
capability sahibinde kalıyor; sahibi onu başka bir yere eşleyebilir
veya Untyped'ına geri döndürmek için `revoke` edebilir.

Yani referans sayacı sorusu **ortadan kalktı**, çözülmedi. ADR 015'in
endişesi "kernel bir tabloyu ne zaman güvenle serbest bırakabilir"
idi; cevap artık "kernel hiçbir zaman serbest bırakmaz, sahibi
bırakır". Kernel'in bilmesi gereken tek şey tablonun ağaçta olup
olmadığı, ve bu tek bir girdi kontrolü.

Bu, kernel'den politika çıkarmanın ADR 011'den beri süren çizgisinin
devamı. **ADR 015 kapatıldı.**

Kalan: kernel'in *kendi* yarısındaki ara tablolar hâlâ bırakılmıyor.
Orada sahip kernel ve soru geri geliyor — ama kernel eşlemeleri boot'ta
kuruluyor ve kaldırılmıyor, yani sorunun pratik bir örneği yok. Bilinen
eksiklere yazıldı.

## Elenen alternatifler

**Dokuzuncu bir `VSpace` nesne tipi.** Kökü diğer tablolardan ayırırdı
ve tip sistemi "bu bir kök" derdi. ADR 017'nin sekiz tipini açmaya
değmedi: köklük bir *bağlanma durumu*, nesnenin bir özelliği değil, ve
aynı tablo bağlanmadan önce sıradan bir tablo.

**Kernel yarısını her VSpace'e yeniden eşlemek.** Paylaşmak yerine
kopyalamak, bir user VSpace'inin kernel eşlemelerini bozmasını
yapısal olarak imkânsız kılardı. Bedeli: kernel bir eşleme
değiştirdiğinde her VSpace'i dolaşmak, ve her VSpace için ~128 PML4
girdisi yerine tam bir tablo ağacı. Kabul edilmedi, ama değişmez K1
onsuz yeterince güçlü değil — bu yüzden K1 test ediliyor, varsayılmıyor.

**Ara tabloları kernel'in ayırması.** Yukarıda; bütçeyi görünmez
kılardı.

**Her switch'te CR3 yazmak.** Basit ve ölçülebilir derecede israf.
Karşılaştırma testin içinde.

## Sonuç

- Bir VSpace bir kök PageTable capability'si; yeni nesne tipi yok.
- Kernel yarısı PML4 seviyesinde paylaşılıyor, GLOBAL, ve boot'taki
  girdi kümesinin değişmediği her VSpace yaratımında doğrulanıyor (K2).
- **K1: bir VSpace'e yalnızca user yarısına eşleme yapılabilir.** İki
  bağımsız kontrol, ikisi de test ediliyor.
- Ara tablolar açıkça sağlanıyor; M3'ün `paging` API'si hiç
  değişmeden, frame vermeyen bir `FrameSource` ile kullanılıyor.
- Haklar eşleme izinlerine çevriliyor: READ yoksa eşlenemez, WRITE
  yoksa RO, GRANT ilgisiz. EXEC M7'de hiç verilmiyor.
- CR3 yalnızca VSpace farklıysa yazılıyor; iki sayaç bunu kanıtlıyor.
- **ADR 015 kapatıldı:** soru çözülmedi, sahipliği devredildi.
- **Uygulamada düzeltildi:** "kökü revoke et" hiçbir şey silmiyordu.
  Kök ve tablolar kardeş; sahip olan Untyped. `revoke_vspace` artık
  onu alıyor.
- **Uygulamada bulundu:** SMAP, USER bitine bakar, CPL'e değil — bu
  yüzden `stac`/`clac` M8'de değil M7'de gerekti. `cpu::UserAccess`
  guard'ı eklendi.
- Ölçüldü: aynı adres uzayında switch **2166 çevrim**, farklı adres
  uzayları arasında **48173** — 22 kat. CR3'ü gereksiz yazmamanın
  bedeli artık bir sayı.
- **Mutasyonla bulundu:** "iki kontrol de test ediliyor" iddiası
  yanlıştı; ikisi aynı hatayı veriyordu ve K1 silinince test geçiyordu.
  Ayrı hata değerleri (`NotUserAddress` / `FlagsWrongHalf`) eklendi.