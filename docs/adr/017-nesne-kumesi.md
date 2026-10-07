# ADR 017 — Nesne kümesi: sekiz tip, ve neden dokuzuncu yok

Status: accepted (M5 architect pass), **ölçümlerle güncellendi (M5
uygulama)**

Kaynak: seL4 Reference Manual (v13), bölüm 2 ve 4. Nereden ödünç
aldığımız ve nerede ayrıldığımız her bölümde ayrı yazılı.

## Bağlam

Capability tabanlı bir mikroçekirdekte **her kernel kaynağı bir
nesnedir** ve her nesneye erişim bir capability üzerinden olur. Nesne
kümesi, kernel'in ne yapabileceğini tanımlar: listede olmayan bir şey
user space'e asla sunulamaz.

Bu küme sonradan büyütülebilir ama **küçültülemez**. Bir nesne tipi
yayımlandığı anda M9'un IPC'si, M11'in VSpace'i ve M13'ün init sistemi
ona göre yazılır; geri almak hepsini etkiler. Bu yüzden eleme ölçütü
şu: *M13'e kadar bu olmadan yapabilir miyiz?* Cevap "evet, biraz
zahmetle" ise listeye girmiyor.

seL4'ün kümesi bundan büyük: Untyped, CNode, TCB, Endpoint,
Notification, Frame, PageTable (ve mimariye özel PageDirectory/PML4
varyantları), IRQControl, IRQHandler, ASIDControl, ASIDPool,
SchedContext, SchedControl, Reply, VCPU, IOPort, IOSpace. Aşağıda
hangilerini aldığımız, hangilerini neden almadığımız var.

## Seçim: sekiz tip

| # | Nesne | Tavan | Ölçülen | Hizalama | Ne zaman gerekli |
|---|---|---|---|---|---|
| 1 | `Untyped` | değişken, 2^k | — | 2^k | M5 — her şeyin kaynağı |
| 2 | `CNode` | 32 B × 2^radix | 32 B/slot | nesne boyutu | M5 — cap'leri tutar |
| 3 | `TCB` | 2048 B | **2048 B** | 2048 B | M7 — thread |
| 4 | `Endpoint` | 32 B | **32 B** | 32 B | M9 — senkron IPC |
| 5 | `Notification` | 64 B | **64 B** | 64 B | M9 — asenkron sinyal |
| 6 | `Frame` | 4096 B | 4096 B | 4096 B | M11 — user belleği |
| 7 | `PageTable` | 4096 B | 4096 B | 4096 B | M11 — user VSpace |
| 8 | `IRQHandler` | 32 B | **32 B** | 32 B | M12 — cihaz kesmesi |

**Ölçüm notu (M5).** Ölçülen sütunu `cap-test` senaryosundan geliyor ve
şu an tavanların tamamına eşit — çünkü nesneler henüz alanları olmayan
yer tutucular, boyutları `cap/mod.rs`'deki sabitlerden geliyor. Gerçek
sınama M7 (TCB), M9 (Endpoint, Notification) ve M12'de (IRQHandler)
olacak: alanlar eklendikçe `size_of` tavana yaklaşacak, ve
`cap/mod.rs`'deki `const` iddiaları aşıldığı anda derlemeyi
durduracak. Bugünkü eşitlik "doğrulandı" değil, "henüz test edilmedi"
anlamına geliyor.

İki tavan mimari geçişten bu yana değişti ve ikisi de burada
güncellendi: TCB 1024 → 2048 bayt (IPC tamponu içeri alındı, açık
soru 5), Notification 48 → 64 bayt (hizalama kuralı gereği zaten 64'e
yuvarlanıyordu; tavanı gerçeğe çekmek daha dürüst).

Tavanlar mimari geçişte tahmin edildi. Tavanı aşan bir değer derleme
hatası (`const _: ()` ile zorlanıyor). Hizalama kuralı her tip için
aynı: **nesne boyutunun bir sonraki ikinin kuvveti kadar**, ki bir
Untyped'dan retype edilirken hizalama aritmetiği tek bir kaydırma olsun
([ADR 020](020-retype-semantigi.md)).

**TCB 1024 bayt ve IPC tamponu içeride.** Mimari geçişte açık
bırakılmıştı (soru 5); karar: tampon TCB'nin içinde, sabit 1 KiB.
Alternatif (seL4 gibi ayrı bir Frame) her IPC'de kernel'in o Frame'e
erişmesini gerektiriyor — PMAP üzerinden fiziksel çözüm ya da kalıcı
bir kernel eşlemesi, ikisi de sıcak yolda. Dolayısıyla TCB'nin gerçek
boyutu **2048 bayt**: 1024 bağlam + 1024 tampon.

Sonucu: **IPC mesajları en fazla 1 KiB.** M13'ün init sistemi dosya
sistemi ya da ağ yığını içermeyecek, birkaç küçük servis ve bir seri
sürücü olacak; 1 KiB yeterli.

### Neden her biri var

**Untyped.** Fiziksel belleğin capability'si. Tüm diğer nesneler bir
Untyped'dan *retype* edilerek doğar. Bu, "kernel kendi adına allocate
etmez" kuralının nesne modelindeki karşılığı: bir TCB yaratmak isteyen,
onun belleğini kendi Untyped'ından vermek zorunda. Kernel'in bir bellek
havuzu yok, dolayısıyla bir bileşenin diğerinin belleğini tüketmesi de
mümkün değil.

**CNode.** Capability'leri tutan dizi. Bir bileşenin sahip olduğu her
yetki bir CNode slot'unda durur, ve CNode'ların kendisi de cap ile
adreslendiği için yetki ağacı oluşur. 32 bayt/slot tahmini: 8 bayt nesne
işaretçisi + 8 bayt tip ve haklar + 16 bayt derivation bağlantıları
([ADR 019](019-derivation-tree.md)).

**TCB.** Yürütme bağlamı: kayıtlar, yığın işaretçisi, zamanlayıcı
durumu, ve bu thread'in CSpace/VSpace kök cap'leri. 1024 bayt, çünkü
x86-64'te kayıt dosyası + FPU/SSE durumu (`fxsave` 512 bayt) tek başına
~700 bayt tutuyor.

**Endpoint.** Senkron IPC randevu noktası. 32 bayt: bekleyen thread
kuyruğunun başı ve kuyruğu, artı durum bitleri. Mesajın kendisi burada
durmuyor — gönderenin TCB'sinde duruyor, çünkü Endpoint'in boyutu mesaj
sayısıyla büyümemeli.

> **M9 architect pass: alanlar somutlaştı.** 32 baytın tamamı
> [ADR 033](033-rendezvous-semantigi.md)'te dağıtıldı: `state: u8`
> (+0), `queue_head: u64` (+8), `queue_tail: u64` (+16), 8 bayt
> ayrılmış. Tahmin tuttu.
>
> Bir sürpriz çıktı ve 32 baytın yetmesinin sebebi o:
> **gönderenler ve alıcılar için iki ayrı kuyruk gerekmiyor.** Bir
> Endpoint'te aynı anda hem gönderen hem alıcı bekleyemez — biri
> varsa diğeri gelir gelmez randevu olur. Tek kuyruk ve bir durum
> alanı yetiyor. İki kuyruk dört işaretçi demek olurdu ve bu tavanı
> delerdi.

**Notification.** Asenkron sinyal. Endpoint'ten ayrı bir tip, çünkü
semantiği farklı: gönderen bloke olmaz, sinyaller bir bit maskesinde
birikir, ve bir kesme bunu doğrudan tetikleyebilir. İkisini tek tipte
birleştirmek, her IPC'de "bu senkron mu asenkron mu" dalı demek olurdu —
sıcak yolda gereksiz.

> **M9 architect pass.** 64 baytın 24'ü kullanılıyor:
> `signalled: u32` (+0), `waiter: u64` (+8), `bound_tcb: u64`
> (+16). Kalan 40 bayt boş.
>
> `bound_tcb` [ADR 038](038-notification-ve-bagli-bekleme.md)'in
> getirdiği alan: bir thread hem bir Endpoint'te bekleyip hem de
> bu Notification'dan uyanabiliyor. M13'ün sürücüsü buna ihtiyaç
> duyuyor — istek Endpoint'ten, kesme Notification'dan geliyor ve
> ikisi için iki thread kullanmak sürücünün durumunu paylaşılan
> hale getirirdi.

**Frame.** User'a eşlenebilir fiziksel sayfa. 4 KiB sabit
([ADR 011](011-sayfa-tablosu-api.md)'in `map`'i yalnızca 4 KiB biliyor).

**PageTable.** User VSpace'in bir seviyesi. x86-64'te dört seviye var
(PML4, PDPT, PD, PT) ama **tek bir tip** kullanıyoruz; gerekçe aşağıda.

**IRQHandler.** Belirli bir kesme hattını bir Notification'a bağlama
yetkisi. M12'ye kadar gerekmiyor ama kümeye şimdi giriyor, çünkü IRQ'nun
*capability ile* yönetileceği kararı mimariyi etkiliyor: kesme
yönlendirmesi kernel politikası olmaktan çıkıp user kararı oluyor.

### PageTable: tek tip, dört seviye

seL4 x86-64'te `PML4`, `PDPT`, `PageDirectory`, `PageTable` diye dört
ayrı nesne tipi tutar. Avantajı: bir PDPT'yi PD slot'una eşlemeye
çalışmak tip hatası olarak yakalanır.

Biz tek tip kullanıyoruz ve kontrolü çalışma zamanına bırakıyoruz:
`map_table(parent, index, table)` çağrısı, `parent`'ın hangi seviyede
eşli olduğuna bakıp `table`'ı bir alt seviyeye yerleştirir. Seviye
bilgisi tablonun kendisinde değil, **nereye eşlendiğinde** saklı.

Gerekçe: dört tip, dört retype yolu, dört invocation kümesi ve dört kod
yolu demek — hepsi birbirinin kopyası. Kazanılan tip güvenliği gerçek ama
küçük, çünkü hata zaten çalışma zamanında yakalanıyor ve bir hata kodu
dönüyor.

**Kapattığımız kapı:** eğer ileride bir seviye diğerlerinden farklı bir
şey tutmak zorunda kalırsa (örneğin PML4'te ASID bilgisi), tipi bölmek
gerekecek ve bu, o tipi kullanan her invocation'ı etkiler.

## Alınmayanlar, ve her birinin bedeli

**ASIDControl / ASIDPool.** seL4'te adres uzayı kimlikleri (PCID) bir
capability ile yönetilir. Biz **PCID kullanmıyoruz**: her CR3 yüklemesi
tam TLB flush demek, ve bu bir performans kaybı. Kabul ediyoruz, çünkü
M13'e kadar birkaç bileşen arasında geçiş yapacağız, binlerce değil.
*Kapattığımız kapı:* PCID eklemek ileride bir nesne tipi daha ve CR3
yükleme yolunun değişmesi demek. Yeniden ele alınacak yer: M11'den sonra,
ölçülmüş bir bağlam-değiştirme maliyeti varsa.

**SchedContext / SchedControl.** seL4'ün MCS dalındaki zaman bütçesi
capability'leri. Zamanın da bir kaynak olduğu ve capability ile
yönetilmesi gerektiği fikri doğru ve zarif. Almıyoruz: M7'de round-robin
bir zamanlayıcı yazacağız ve bütçeli zamanlama ayrı bir araştırma konusu.
*Kapattığımız kapı:* gerçek zamanlı garantiler. Bu kernel M13'te "bu
thread 10 ms içinde çalışacak" diyemeyecek.

**Reply (MCS).** Çağrı-cevap IPC'sinde cevap hakkını temsil eden nesne.
seL4'ün MCS olmayan dalı bunu TCB'nin içinde örtük tutar; biz de öyle
yapacağız ([ADR 021](021-capability-haklari.md), `grantreply`).
*Kapattığımız kapı:* cevap hakkının devredilebilmesi — bir sunucunun "bu
isteği başkası cevaplasın" diyememesi.

> **M9 architect pass: "örtük" somutlaştı.**
> [ADR 036](036-call-reply.md) cevap hakkını **sunucunun
> TCB'sinde** bir alan yaptı (`reply_to` + `reply_generation`),
> çağıranın TCB'sinde değil. Sebep: sunucu `Reply` dediğinde kime
> cevap vereceğini O(1)'de bulmalı, ve o bilgi çağıranda olsaydı
> onu aramak gerekirdi.
>
> Ayrı bir Reply nesnesine karşı bir gerekçe daha çıktı: **her
> `Call` bir allocation olurdu**, çünkü nesne bir Untyped'dan
> retype edilmeli. Sıcak yolda.

**VCPU.** Sanallaştırma. Kapsam dışı.

**IOPort / IOSpace.** x86 I/O portlarına ve IOMMU'ya capability ile
erişim. M12'de bir cihaz sürücüsü yazarken gerekebilir — ama o sürücü
MMIO kullanabiliyorsa gerekmeyecek. Ertelendi; M12'de karar.

**IRQControl.** seL4'te IRQHandler üretme yetkisi ayrı bir capability.
Biz tek bir IRQControl cap'ini init bileşenine verip orada bırakıyoruz —
ayrı bir *tip* değil, IRQHandler cap'inin ayrıcalıklı bir varyantı. Bir
tip kazancı, bir kavram kaybı değil.

**Kendi düşünüp eklemediklerimiz.** Bir "Memory" nesnesi (Untyped'ın alt
tipi), bir "ThreadGroup", bir "Logger" düşünüldü ve hepsi elendi:
hiçbiri mevcut sekizin kombinasyonuyla yapılamayacak bir şey sağlamıyor.

## Kümenin M13'e kadar yetip yetmediği

| Milestone | İhtiyaç | Küme karşılıyor mu |
|---|---|---|
| M5 | Untyped, CNode, retype, revoke | Evet |
| M6 | APIC kernel içi | Nesne gerekmez |
| M7 | Thread | TCB |
| M8 | Ring 3 | TCB + Frame + PageTable |
| M9 | IPC | Endpoint + Notification |
| M10 | Scheduler | TCB (öncelik alanı) |
| M11 | User VSpace | PageTable + Frame |
| M12 | Cihaz sürücüsü | IRQHandler + Frame (MMIO) + Notification |
| M13 | Init sistemi | Hepsi |

### MMIO: device Untyped, dokuzuncu tip değil

M12'de bir cihaz sürücüsü register penceresine erişecek. Çözüm yeni bir
nesne tipi değil: **Untyped'a bir `device` bayrağı.**

Boot sırasında kernel, memory map'teki RAM-olmayan bölgeler için
**device Untyped**'lar üretir. Normal Untyped'dan tek farkı:

> **Değişmez: bir device Untyped yalnızca `Frame`'e retype edilebilir.**
> TCB, CNode, Endpoint, Notification, PageTable ya da IRQHandler
> denemesi `DeviceUntypedFrameOnly` döner.

Gerekçe tek cümleyle: **kernel nesneleri kernel'in okuyup yazdığı
yapılardır, bir cihaz register penceresi öyle davranmaz.** Bir
Endpoint'in kuyruk işaretçisini bir cihazın durum register'ının üzerine
yazmak hem nesneyi bozar hem cihaza rastgele komut gönderir; belirtisi
donanıma bağlı ve teşhisi imkânsıza yakın.

Bir bileşen rastgele bir fiziksel adresi Frame'e çeviremez: Frame
yalnızca retype ile doğar, retype bir Untyped gerektirir, Untyped'ları
yalnızca boot kodu üretir.

Device Frame'leri M12'de `NO_CACHE` ile eşlenecek — bu bir eşleme
bayrağı, nesne özelliği değil.

`cap-test` kontrol 12 bunu doğruluyor.

## Elenen alternatifler

**Daha büyük bir küme (seL4'ün tamamı).** Her tip; retype yolunda bir
dal, invocation tablosunda bir satır, ve M13'e kadar test edilmesi
gereken bir yüzey. Yirmi tiple başlamak, hiçbirini iyi yapmamak demek.

**Daha küçük bir küme (Untyped + CNode + TCB + Frame).** Endpoint'i
TCB'nin içine, Notification'ı bir bit maskesine, PageTable'ı Frame'in
özel bir kullanımına indirgemek mümkün. Sonuç: IPC semantiği TCB'ye
gömülü, yani değiştirilemez; ve Frame'in "veri mi tablo mu" olduğu
çalışma zamanında belirsiz. Tip ayrımının bedeli küçük, kazancı açık.

**Nesne boyutlarını çalışma zamanında belirlemek.** `TCB` 1024 bayt
yerine "ne kadar gerekiyorsa" olabilirdi. Retype aritmetiğini
karmaşıklaştırır ve hizalama garantisini kaybettirir. Sabit boyut,
`size_of` kadar basit.

## Sonuç

- Sekiz nesne tipi. Her biri bir milestone tarafından gerekli kılınıyor;
  hiçbiri "ileride lazım olur" diye eklenmedi.
- PageTable tek tip, seviye eşlendiği yerde belli. seL4'ten ayrıldığımız
  birinci nokta.
- PCID, bütçeli zamanlama, devredilebilir Reply ve sanallaştırma kapsam
  dışı — her birinin kapattığı kapı yukarıda adlandırıldı.
- Boyutlar tavan olarak verildi; `cap-test` gerçek `size_of` değerlerini
  basıyor ve tavanı aşmak derleme hatası.
- Hizalama kuralı tek: nesne boyutunun bir sonraki ikinin kuvveti.
- **TCB 2048 bayt**: 1024 bağlam + 1024 IPC tamponu. IPC mesajları
  ≤ 1 KiB.
- **MMIO = device Untyped**, dokuzuncu tip değil. Değişmez: device
  Untyped yalnızca Frame'e retype edilebilir, ve `cap-test` bunu
  doğruluyor.