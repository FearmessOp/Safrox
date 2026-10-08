# ADR 043 — User-space VSpace yönetimi: CSpace thread'e iniyor

Status: accepted (M11)

Kaynak: seL4 Reference Manual (v13), bölüm 3 (CSpace) ve 7 (Address
Spaces); ADR 027 (VSpace modeli), ADR 030 (syscall ABI), ADR 022
(kernel'in ayrıcalıklı kaynakları).

M7 bir adres uzayını capability'lerden kurdu — ama **kernel içinden**.
M8 ring 3'ü açtı, M11 ikisini birleştiriyor: bir user bileşeni kendi
adres uzayını kendi yönetiyor.

## Soru

M7'nin `map_frame`, `map_table`, `unmap_*` fonksiyonları zaten
capability alıyor ve zaten hak kontrolü yapıyor. Onları syscall
dispatcher'ına bağlamak kaç satır?

Birkaç satır. Ve bu, sorunun yanlış sorulduğunun işareti.

Gerçek soru şu: **bir user bileşeni hangi capability'leri
adlandırabilir?** M8'den beri dispatcher her CPtr'yi init CSpace'inde
çözüyor:

```rust
let space = crate::cap::boot::init_cspace();
```

Bu, M8 ve M10 için doğruydu, çünkü oradaki user thread'leri *testin
kendi* thread'leriydi. M11'de yanlış hâle geliyor ve nedeni bir
özellik eksikliği değil, bir **izolasyon deliği**: tek bir CSpace
varsa, her thread her capability'yi adlandırabilir. B bileşeni A'nın
PageTable'ının CPtr'sini tahmin edip A'nın adres uzayına eşleme
yapabilir.

Negatif senaryo (b) — "user kodu başka bir bileşenin VSpace'ine
eşleme deniyor" — bu durumda **başarısız olamaz**. Test yazılabilir,
geçer, ve hiçbir şey kanıtlamaz. ADR 023'ün K5'i tam olarak bunu
yasaklıyor: kendi yokluğundan ayırt edilemeyen bir kontrol, kontrol
değildir.

Yani M11'in asıl işi sayfa tablosu invocation'ları değil. **CSpace'i
thread'e indirmek.**

## Karar 1: her TCB'nin kendi CSpace kökü var

```
Tcb {
    ...
    cspace_root: u64,       // kok CNode'un fiziksel adresi, ya da 0
    cspace_slot: u64,       // capability'nin bulundugu slotun adresi
    cspace_generation: u8,  // o slotun generation'i (ADR 018)
    cspace_radix: u8,
}
```

`vspace_root`/`vspace_slot`/`vspace_generation` üçlüsünün birebir
aynısı, ve bu tesadüf değil: ikisi de "bu thread'in şu nesneye bağlı
olduğu, ve o nesnenin hâlâ aynı nesne olduğu" bilgisini taşıyor.
ADR 018'in CPtr'lerinde, ADR 028'in VSpace'lerinde, ADR 036'nın reply
hedeflerinde ve M10'un IRQ handler'ında kullanılan aynı deyim —
adres artı generation.

Sıfır **kernel'in CSpace'i** demek, yani init CSpace'i. Bu bir
geçici çözüm değil, bir tanım: boot thread'i ve kernel test
thread'leri init'in kendisi, ve init'in CSpace'i init CSpace'i.

### Çözümleme bu kökten başlıyor

```rust
let space = current_cspace();   // thread'in kendi koku
let slot = resolve(&space, cptr)?;
```

Bir bileşen, CSpace'inde bulunmayan bir capability'yi **adlandıramaz**.
Yasak değil — *ifade edilemez*. CPtr bir indeks çifti, ve o indeksler
thread'in kendi kökünden yürüyor. Başka bir bileşenin PageTable'ı
oraya girmiyorsa, onu gösteren bir CPtr yok.

Bu, ADR 035'in capability transferi için verdiği argümanın aynısı:
"bir gönderici alıcının CSpace'inde bir hedef adlandıramaz, çünkü
adlandıracak bir alan yok." Orada güvenli bir varsayılan değil,
yapısal bir imkânsızlıktı. Burada da öyle.

### Maliyeti: thread switch'te hiçbir şey

CSpace kökü CR3 gibi yüklenmiyor; sadece bir alan. Syscall başına bir
okuma ekliyor — `init_cspace()` zaten bir statik okumaydı, şimdi
TCB'den bir alan okuması. Aynı sayıda bellek erişimi.

## Karar 2: ABI iki register daha kullanıyor

Sorun somut: `Frame.Map` dört şey istiyor — hangi Frame, hangi
VSpace, hangi adres, hangi izinler. ADR 030'un ABI'sinde RDI hedef
CPtr, RSI metod; geriye RDX ve R10 kalıyor. Üç argüman gerekiyor, iki
yer var.

Üç seçenek vardı:

**(a) İzinleri adresin alt bitlerine paketlemek.** Adres sayfa
hizalı, yani alt 12 bit boş. Çalışır, ve yanlış: `0x400000 | 2`
ifadesi bir adres değil, ve bir gün birisi onu adres sanıp
maskelemeyi unutacak. Kodun okunabilirliği bir ABI kararının bedeli
olmamalı.

**(b) IPC tamponunu kullanmak.** seL4 bunu yapıyor (message
register'lar). Burada yanlış: IPC tamponu TCB'nin içinde ve bir
invocation'ın onu kirletmesi, aynı thread'in devam eden bir IPC'sini
bozar. Ayrıca bir bellek erişimi ekliyor, register okuması yerine.

**(c) R8 ve R9'u kullanmak.** **Seçilen.**

ADR 034 zaten bunları saymıştı: *"`syscall` RCX ve R11'i yok ediyor,
geriye RDX, R10, R8 ve R9 kalıyor."* IPC `Send` üçünü mesaj kelimesi
olarak kullanıyor ve dördüncüyü kullanmıyor; invocation yolu ise
şimdiye kadar yalnızca ikisini kullanıyordu. Register'lar oradaydı.

```
RDI  cptr        hedef capability
RSI  method
RDX  arg0
R10  arg1
R8   arg2        <- M11
R9   arg3        <- M11
```

Dört argüman. `Frame.Map` üçünü kullanıyor, `Untyped.Retype` dördünü.

Stub zaten on beş register'ın hepsini kaydediyor (ADR 030: *"bir
register kaydedilmezse user'ın değeri kernel'e sızar, geri
yüklenmezse kernel'in değeri user'a sızar"*), yani çerçeve değişmiyor
— sadece dispatcher iki alan daha okuyor.

## Karar 3: hangi invocation'lar açılıyor

| invocation | hedef | argümanlar |
|---|---|---|
| `Frame.Map` | Frame | root cptr, adres, izinler |
| `Frame.Unmap` | PageTable (root) | adres |
| `PageTable.Map` | PageTable | root cptr, adres, seviye |
| `PageTable.Unmap` | PageTable (root) | adres, seviye |
| `Untyped.Retype` | Untyped | tip, size_bits, sayı, hedef (cnode cptr + offset) |
| `TCB.SetSpace` | TCB | vspace cptr, cspace cptr |
| `TCB.Resume` | TCB | — |

Hepsi M7'nin mevcut fonksiyonlarını çağırıyor. **Yeni hiçbir kernel
mantığı yok** — ve bu bir iddia, ADR 011'in "mekanizma politika
değildir" kararının ödülü: fonksiyonlar baştan capability alacak
şekilde yazıldığı için, user'a açmak bir çözümleme katmanından
ibaret.

### `Unmap`'in hedefi Frame değil, root

`Frame.Unmap`'in doğal okunuşu "bu Frame'i eşlemeden çıkar". Yanlış
olurdu: aynı Frame birden çok adres uzayına eşlenebiliyor (ADR 027),
ve capability hangisinden çıkarılacağını söylemiyor. Hedef **adres
uzayı**, argüman adres.

`PageTable.Unmap` da öyle, ve aynı sebeple.

### `Retype` user'a açılıyor — ve bu bütçe argümanının sınavı

ADR 022'nin iddiası şu: kernel kendine bellek ayırmıyor, her
allocation birinin bütçesinden çıkıyor. M5'ten beri doğru, ama
**kernel içinden** doğru. Bir bileşen kendi Untyped'ından retype
ettiğinde iddia ilk kez gerçek anlamını kazanıyor: bir bileşen
belleği tükettiğinde **yalnızca kendi** belleği tükeniyor.

`vspace-user-exhaust` bunu test ediyor: user kodu Untyped'ını bitene
kadar PageTable üretiyor, temiz bir `NotEnoughMemory` alıyor, ve
kernel'in kendi belleğinin etkilenmediği ayrıca kontrol ediliyor.
Tek bir bileşenin sistemi aç bırakamaması — mikroçekirdek
iddiasının ölçülebilir hâli.

## Karar 4: ADR 015'in kernel tarafı kapanıyor

ADR 015 şunu sormuştu: `unmap` ara sayfa tablolarını serbest
bırakmalı mı? M7 user tarafını kapattı — tablolar artık PageTable
capability'si, `Unmap` onları sahibine geri veriyor, kernel hiçbir
zaman sahip olmadı.

Kernel yarısı açık kalmıştı. Şimdi kapanıyor, ve cevap "serbest
bırakmıyoruz" değil, **"sorunun bir örneği yok"**:

Kernel'in sayfa tabloları boot'ta kuruluyor ve hiç kaldırılmıyor.
Heap yalnızca büyüyor (ADR 016), KSTACK slotları sabit, PMAP ve image
eşlemeleri kalıcı. `unmap` kernel yarısında **çağrılmıyor**.

Peki bir gün çağrılırsa? En olası aday ölü bir thread'in yığınını
kaldırmak. O gün geldiğinde cevap user tarafının cevabı olacak:
tabloyu sahibine geri ver. Çünkü o yığın bir TCB'ye ait, TCB bir
Untyped'dan geliyor, ve Untyped revoke edildiğinde tablolar da
dönüyor — ADR 020'nin watermark kararı bunu zaten çözüyor.

Yani ADR 015'in sorusu "kernel bir tabloyu ne zaman serbest
bırakmalı" idi, ve cevap **"kernel bir tabloya hiç sahip olmamalı"**.
M7 bunu user tarafında yaptı; kernel tarafında zaten öyle, çünkü
kernel'in tabloları hiç serbest bırakılmıyor. Soru kapsam yoluyla
kapanıyor, uygulama yoluyla değil — ve bu bir erteleme değil, bir
cevap: ADR 015 artık *closed* işaretli.

## Elenen alternatifler

**CSpace'i VSpace gibi CR3'e benzer bir yere koymak.** CSpace bir
donanım kavramı değil; her syscall'da okunuyor ve hiçbir zaman CPU
durumuna yüklenmiyor. Bir TCB alanı tam olarak doğru yer.

**Tek CSpace bırakıp "bileşenler birbirinin CPtr'sini tahmin
edemez" demek.** Güvenlik belirsizlikle. CPtr'ler tahmin edilebilir —
indeks çiftleri — ve generation sadece *stale* olanı yakalıyor,
*yabancı* olanı değil.

**`Retype`'ı user'a kapalı tutmak.** O zaman bir bileşen kendi
nesnelerini üretemez ve her şeyi init'ten istemek zorunda kalır.
Bu, init'i bir allocator yapar — ve ADR 020'nin Untyped'ı allocator
olmaktan kaçınma kararını tersine çevirir.

**İzinleri ayrı bir invocation'la ayarlamak** (`Frame.SetRights`).
İki çağrı, arada bir pencere, ve "eşlendi ama izinleri henüz
ayarlanmadı" diye bir durum. Tek çağrı, tek karar.

## Sonuç

- **Her TCB'nin kendi CSpace kökü var** (adres + slot + generation,
  ADR 018'in deyimi). Çözümleme oradan başlıyor. Bir bileşen
  CSpace'inde olmayan bir capability'yi adlandıramıyor — yasak
  değil, ifade edilemez.
- Bu, M11'in asıl işi. Sayfa tablosu invocation'larını user'a açmak
  birkaç satır; onları **anlamlı** kılan CSpace ayrımı.
- **ABI R8 ve R9'u kullanıyor.** ADR 034 bunları zaten saymıştı.
  Adresin alt bitlerine paketleme ve IPC tamponunu kullanma
  reddedildi.
- Yedi invocation açılıyor, **hiç yeni kernel mantığı yok** —
  ADR 011'in mekanizma/politika ayrımının ödülü.
- `Unmap`'in hedefi adres uzayı, Frame değil: aynı Frame birden çok
  yere eşlenebiliyor.
- `Retype` user'a açılıyor, ve ADR 022'nin bütçe iddiası ilk kez
  gerçek anlamında test ediliyor: bir bileşen yalnızca kendi
  belleğini tüketebiliyor.
- **ADR 015 kapanıyor.** Kernel bir tabloya hiç sahip olmamalı; user
  tarafında M7 bunu yaptı, kernel tarafında sorunun bir örneği yok ve
  bir gün olursa cevabı aynı.

## Uygulama sırasında değişenler

Bu bölüm sonradan eklendi. Yukarıdaki kararlar yazıldıkları gibi
duruyor; aşağıdakiler onları **yanlışlayan** ya da eksik bırakan
bulgular, ve bir kısmı karardan daha öğretici.

### Yedi invocation yetmedi: 12 ve 13

Karar 3 yedi invocation sayıyordu ve "hiç yeni kernel mantığı yok"
diyordu. İkinci iddia doğru çıktı; birincisi değil. Senaryo 5'i —
bir bileşenin ikinci bir bileşen kurması — yazmaya başlayınca iki
şeyin ifade edilemez olduğu görüldü:

**`PageTable.CreateVSpace` (12).** Bir bileşen PageTable retype
edip onu *ara tablo* olarak haritalayabiliyor, ama bir tanesini
**kök** yapamıyor. Taze bir PML4 bir adres uzayı değildir: user
yarısı boştur (istenen budur), ama kernel yarısı da boştur — ve
orada çalışan bir thread bir kesmeyi alamaz, çünkü handler eşli
değildir. Bileşen bunu kendisi düzeltemez; yazması gereken girdiler
kernel belleğini adlandırır ve elinde hiçbiri için capability
yoktur. M7'nin `create_vspace`'i değişmeden çağrılıyor, yani "yeni
mantık yok" iddiası ayakta — ama "yedi yeter" iddiası değildi.

**`TCB.SetUserRegisters` (13).** M6'nın `SetRegisters`'ı bir Rust
`fn` işaretçisi alıyor, yani bir kernel adresi — bir user thread'in
başlamaması gereken tam da o şey.

İkincisi bir sınırı görünür kıldı. Kernele giren bir thread'in
**kernel yarısında** bir yığına ihtiyacı var, ve kernel-yarısı
belleği hiçbir capability tarafından adlandırılmıyor (ADR 027'nin
K1'i, öbür taraftan bakıldığında). Yani bileşen adresi veremez.

Verdiği şey **frame'ler**: `SetUserRegisters` kendi Untyped'ından
retype ettiği dört Frame'i bir CNode'da sunuyor, kernel sanal
aralığı seçip oraya haritalıyor. Bellek üzerindeki yetki bileşende
kalıyor, adres uzayı üzerindeki yetki kernelde — ve ADR 022'nin
"kernel kendine bellek ayırmıyor" iddiası bir bileşen thread
yarattığında da ayakta kalıyor.

### `NoKernelStack`: derleyicinin yakaladığı ayrım

KSTACK bölgesi tükendiğinde ne dönecek? İlk yazım
`NotEnoughMemory` koydu ve derlemedi — o varyant alan taşıyor.

Zorunlu düzeltme doğru soruyu sordurttu. `NotEnoughMemory`
**"senin Untyped'ın bitti"** demek: bileşenin kendi bütçesi, ADR
022'ye göre bir bileşenin tüketebileceği tek şey. KSTACK ise
kernelin, yani **paylaşılan** bir kaynak — ve bu daha zayıf bir
garanti.

İkisini aynı koda sıkıştırmak `vspace-user-exhaust`'u belirsiz
yapardı: paylaşılan havuzu tüketen bir koşu, kendi belleğini
tüketen bir koşuyla aynı kodu döndürürdü, ve test edilen iddia tam
olarak *hangisi* olduğuydu. Yeni kod 30; test 11'i isimle
istiyor.

### Yedi değil dokuz; ve ADR 015 yine de kapanıyor

Karar 4 duruyor. `unmap_table` M7'den beri tabloyu sahibine geri
veriyor, kernel hiçbir zaman sahip olmuyor, ve kernel yarısında
sorunun bir örneği yok. **ADR 015 kapalı.**

## Testin kendisi hakkında bulgular

Dört tanesi kernel hakkında değil, **testin ne kanıtladığı**
hakkında. ADR 023'ün K5'i bunları kernel bulguları kadar ciddiye
almayı gerektiriyor.

### `vspace-user-foreign` hiçbir şey kanıtlamıyordu

En önemlisi bu. İlk yazım B'ye A'nın VSpace CPtr'sini veriyor ve
B'nin başarısız olmasını bekliyordu. B **başarılı oldu**, ve test
"iki bileşen izole değil" diye bağırdı.

İzolasyon çalışıyordu. Hatalı olan testti.

A ile B'nin CSpace'leri aynı şekilde: ikisinin de VSpace'i kendi
leaf'lerinin 1. slotunda. Generation'lar sıfır olduğundan **aynı
bitler** — `0x1` — her iki CSpace'te de geçerli ve her birinde
*kendi* VSpace'ini adlandırıyor. B kendi adres uzayına haritaladı
ve haklı olarak başardı.

Buradaki ders ADR 018'in modelinin bir sonucu ve ilk taslakta
kaçırılmıştı: **bir CPtr başka bir CSpace'te hiçbir şey demek
değildir diye bir kural yok.** Orada ne varsa onu adlandırır. Yani
izolasyonu göstermek için asimetriyi *kurmak* gerekiyor — A'nın
VSpace'i için B'nin hiç kullanmadığı bir indekste ikinci bir
capability (`A_ONLY_VSPACE`, 40). B'nin o indeksteki slotu boş, ve
sonuç `EmptySlot`: yasak değil, **yok**.

Testin şimdi iddia ettiği de bu: hata 3, ve `map_frame`'in
içinden gelen *herhangi* bir hata reddediliyor. Çözümleme başarılı
olup sonra bir kontrolün hayır demesi — reddetme yoluyla izolasyon
— işaretçinin şeklinden gelen izolasyondan zayıftır. Birincisi
silinebilecek bir kontrol, ikincisi silinemez.

### Havuzlanmış adımda hata üzerine yazılıyordu

Çocuğun kurulumu yirmi küsur invocation ve hepsi tek bir rapor
slotuna yazıyordu, "son yazan kazanır" kuralıyla. Koddaki yorum
"sonuncusu başarılıysa hepsi başarılıdır" diyordu, ki yanlış.

Semptom bir assert değil, **ring 3'te bir #PF** oldu: ebeveynin
çocuk kod frame'ini haritalaması `MissingPageTable` ile başarısız
oldu, sonraki bir invocation hatayı sıfırla ezdi, ve ebeveyn var
olmayan bir haritalamaya yazdı.

`record_step` artık **ilk hatayı saklıyor**. Bir adım ancak ona
rapor veren *her* invocation başarılıysa başarı okunuyor — havuzun
zaten anlamı buydu.

### Resume'dan sonra beklemeden ölçmek (K8)

`Resume` çocuğu hazır kuyruğuna koyup döner; ebeveyn DONE der;
test uyanır ve çocuğun raporunu arar. O anda çocuk
*zamanlanmıştır*, *çalışmamıştır*. İlk yazım hemen assert etti ve
"çocuk hiç çalışmadı" dedi — testin kendi yarattığı bir yarış için
`Resume`'u suçlayarak.

### Fixture verdiği bütçeyi saymamıştı (K6)

`untyped_with_room` 256 KiB istiyordu ve bileşene verilen bütçe de
256 KiB'ti. İlk TCB retype'ı `NotEnoughMemory { remaining: 0 }`
ile öldü. Bir fixture neye ihtiyacı olduğunu söyler — ve "neye
ihtiyacı olduğu" verdiği şeyi de içerir.

### Kapsamı görmek: 2 MiB'de PT yoktu

`SCRATCH_ADDRESS` 8 MiB'ydi ve yorumu "PDPT ve PD'yi paylaşıyor,
tablolar zaten orada" diyordu. İlki doğru, ikincisi değil: bir PD
paylaşmak 1 GiB'lık bir bölgeyi paylaşmak demek, bir haritalamanın
ihtiyacı olansa kendi **2 MiB**'ı için PT. Fixture 4, 5 ve 6
MiB'ı haritalıyor, yani PT'ler 4–6 ve 6–8'i kapsıyor; 8 MiB
üçüncü bir tabloya düşüyordu.

İki farklı granülerlik, ve kaba olanı güven verici okunan.

## K5 mutasyonu: öldü, ama öğretici biçimde

Zorunlu mutasyon `map_frame`'deki `NotUserAddress` kontrolünü
kaldırmak. `vspace-user-kernel-half` **kırıldı** — ama beklenen
sebeple değil.

Haritalama yine reddedildi. Paging katmanı USER bayrağının yarıyla
uyuşmadığını gördü ve `FlagsWrongHalf` (16) döndürdü; test 15
bekliyordu ve o farkta patladı.

Yani mutasyon yakalandı, ama **öteki kontrol tarafından**. İki
kontrol aynı hata kodunu paylaşsaydı mutasyon hayatta kalırdı ve
bir okuyucu birinin gereksiz olduğu sonucuna varırdı. ADR 027 her
ikisini de tutmayı savunmuştu ve M7 mutasyonla ayırt
edilebilirliklerinin şart olduğunu bulmuştu; bu, aynı bulgunun
user yolundan gelişi.

Kontroller neyi koruduklarıyla ayrışıyor. Bunu kaldırınca kernel
yarısı *bugün* hâlâ güvenli, çünkü her user haritalaması USER
bayrağını taşıyor. Bir gün bir şey user VSpace'ine o bayrak
olmadan haritalarsa güvenli olmaktan çıkar — ve hiçbir şey fark
etmez.