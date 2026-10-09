# ADR 050 — Boot'ta device Untyped tohumlama

- **Durum:** Kabul edildi
- **Tarih:** M14
- **Bağlam:** ADR 017 (nesne kümesi, "MMIO: device Untyped"),
  ADR 020 (retype), ADR 022 (kernel kendi adına allocate etmez),
  ADR 049 (BootInfo'da framebuffer)

## Bağlam

ADR 017, M5'te şunu yazdı:

> Boot sırasında kernel, memory map'teki RAM-olmayan bölgeler için
> **device Untyped**'lar üretir.

O cümle M5'te yazıldı ve **uygulanmadı**. Mekanizmanın tamamı var —
`Untyped`'ın `device` bayrağı, `DeviceUntypedFrameOnly` hatası,
bayrağın retype sırasında Frame'e miras kalması, `cap-test`'in 12.
kontrolü — ama tek çağıran `captest.rs`: test, yapay bir Untyped
yaratıp `mark_device` ile işaretliyor.

Yani M5'ten M13'e kadar sistemde **gerçek bir device Untyped hiç
olmadı.** Mekanizma doğru, test ediliyor, ve hiçbir aygıtı temsil
etmiyor.

M14 bunun ilk gerçek kullanıcısı. Yeni mekanizma eklenmiyor; var
olanın ilk müşterisi geliyor.

## Karar

Boot, memory map'te `MEMMAP_FRAMEBUFFER` olarak işaretli bölgeyi bir
**device Untyped** olarak tohumlar ve root task'ın CSpace'ine koyar.
Hangi slot olduğu BootInfo'da (ADR 049).

### Neden yalnızca framebuffer

Memory map'te RAM-olmayan başka bölgeler de var: `reserved`, ACPI NVS,
`bad memory`. Hiçbiri tohumlanmıyor ve sebebi ADR 017'nin eleme
ölçütünün aynısı: *bunu şimdi kullanan bir şey var mı?*

- **Framebuffer**: evet, M14'ün ekran bileşeni.
- **Diğerleri**: hayır. ACPI tabloları M15+ bir güç yönetimi bileşeni
  isterse gerekecek; `reserved` bölgeleri çoğunlukla firmware'in
  kendi işi. Bugün tohumlamak, hiçbir şeyin istemediği capability'ler
  üretmek demek — ve dağıtılmamış yetki, dağıtılmış yetkiden daha
  güvenli değil, sadece daha görünmez.

Tohumlama kodu bölge *türüne* göre yazılıyor, framebuffer adresine
göre değil; ikinci bir tür eklemek bir satır.

### Boyut ve hizalama

Bir Untyped `2^k` bayt ve `2^k` sınırında olmak zorunda (ADR 020).
Framebuffer bölgesi ikisini de sağlamayabilir.

Çözüm, normal belleğe uygulanan **aynı** carve döngüsü: bölgeyi
hizalanmış ikinin-kuvveti bloklara böl, her biri bir Untyped. Ayrı bir
kod yolu değil — aynı döngü, `device` bayrağı set edilmiş.

Bu, bir framebuffer'ın birden fazla Untyped'a bölünebileceği anlamına
geliyor. Root task'ın işi: BootInfo'daki slot **ilkini** gösteriyor ve
ardışık olanlar aynı bölgenin devamı. 1024×768×32 = 3 MiB, yani
tipik olarak birkaç blok.

### Frame allocator'a sorulmuyor

Normal bellek tohumlanırken her frame `frame_allocator`'a sorulur:
"bu frame boş mu, yoksa çekirdeğin imajı/yığını/bitmap'i mi?" Device
belleği için bu soru **anlamsız** — framebuffer allocator'ın
bitmap'inde hiç yok, çünkü bitmap yalnızca RAM'i kapsıyor.

Sorulsaydı cevap "boş değil" olurdu ve hiçbir şey tohumlanmazdı. Bu
sessiz bir başarısızlık olurdu: kod çalışır, Untyped üretmez, ve
belirtisi "framebuffer slot'u sıfır".

Dolayısıyla device bölgeleri allocator'ı atlıyor, ve **bu atlamanın
kendisi bir değişmez**: device belleği asla `allocate()`'ten
gelmediği için `retire()`'dan sonra da ulaşılabilir kalır, ve
çekirdeğin bellek muhasebesine (`usable_bytes`) **girmez**. M12'nin
muhasebe denkliği bozulmuyor çünkü framebuffer o denklemin hiçbir
teriminde yok.

## Elenen alternatif: düz fiziksel adres

En kolay yol: BootInfo'ya framebuffer'ın fiziksel adresini ve boyutunu
koymak, root task'a "şurayı eşle" demek. Capability yok, Untyped yok,
tohumlama yok.

**Reddedildi, iki gerekçeyle.**

**1. Düz bir adres referans değil, bir sayıdır.** Capability sisteminin
tamamı, yetkinin *adlandırılabilir* olmasına dayanıyor: bir CPtr bir
köke göre çözülür (ADR 018), kopyalanabilir, badge'lenebilir,
daraltılabilir, iptal edilebilir. `0xFD000000` sayısı bunların
hiçbiri değil. Onu BootInfo'ya yazmak, capability sisteminin yanından
geçen ikinci bir yetki kanalı açmak demek — ve iki yetki kanalı olan
bir sistemde, hangisinin neye izin verdiğini söylemek artık tek bir
yerde yapılamaz.

Bu, ADR 045'in IOPL'i reddetme gerekçesinin aynısı: yetki,
capability olmayan bir biçimde verilirse, capability sisteminin
söyledikleri o yetki hakkında hiçbir şey söylemez.

**2. `DeviceUntypedFrameOnly` kısıtı kaybolurdu.** Düz adres
yaklaşımında root task o aralığı eşlemek için bir Frame üretmek
zorunda, ve Frame'i *kendi* normal Untyped'ından üretirdi — fiziksel
adresi zorlayarak. O Frame `device` bayrağı taşımazdı. Sonuç: aynı
bellekten TCB üretilebilir hale gelirdi, ki ADR 017'nin bütün
gerekçesi (bir Endpoint'in kuyruk işaretçisini aygıt register'ının
üzerine yazmak) tam olarak bunu engellemek içindi.

Yani düz adres yalnızca "daha az zarif" değil; M5'te verilmiş bir
güvenlik kararını sessizce geri alırdı.

## Elenen alternatif: dokuzuncu nesne tipi

Bir `Framebuffer` nesne tipi. ADR 017 bunu zaten eledi ve gerekçe
değişmedi: device Untyped + Frame kombinasyonunun yapamadığı bir şey
sağlamıyor, ve her yeni tip retype yolunda bir dal, dispatch
tablosunda bir satır, `release_ipc_references`'ta bir arm demek.

## Sonuçlar

- M5'in kararı M14'te uygulandı. ADR 017'nin "boot device Untyped
  üretir" cümlesi artık bir niyet değil, bir olgu.
- `cap-test`'in 12. kontrolü yapay bir Untyped'ı sınamaya devam
  ediyor **ve** `fb-device-untyped-misuse` aynı kısıtı gerçek bir
  aygıtla sınıyor. İkisi ayrı: biri mekanizmayı, diğeri tohumlamanın
  bayrağı gerçekten set ettiğini gösteriyor.
- Device belleği bellek muhasebesine girmiyor ve girmemeli.
- Framebuffer birden fazla Untyped olabilir; BootInfo ilkini verir.

---

## Düzeltme — "muhasebe capability'de yaşar" cümlesi yanlıştı

Bu ADR'nin ilk hâli, `boot.rs`'deki tohumlama döngüsünün yorumunda
şunu yazdı:

> Bir device Untyped'ın muhasebesi capability'de yaşar, adlandırdığı
> bellekte değil — ki `size_bits`'in slot'ta olmasının sebebi bu.

**Cümle iki ayrı biçimde yanlıştı ve silinmiyor, düzeltiliyor.**
Silmek, yanlışın yapılmış olduğunu da silerdi.

**Birinci yanlış: `size_bits` bunu yapamaz.** `size_bits` bölgenin
*boyutu* ve hiç değişmiyor. Watermark her retype'ta değişiyor.
Değişmeyen bir alan, değişen bir sayıyı tutamaz. Cümle iki farklı
büyüklüğü birbirine karıştırdı.

**İkinci yanlış, ve asıl olan: capability yanlış yer.** Bir Untyped
capability'si **kopyalanabilir** — `copy` bunu yapar ve M14'te
`wire_screen` tam olarak bunu yapıyor. Watermark capability'de
yaşasaydı, her kopyanın kendi sayacı olurdu ve iki kopya **aynı
baytları iki kez dağıtırdı**.

Doğru ifade şu: **watermark nesneye aittir, capability'ye değil.**
Bu, RAM için neden nesnenin kendi belleğinde durduğunu da açıklıyor —
oradaki konum bir uygulama detayı değil, "bütün kopyalar aynı sayıyı
görmeli" kuralının bir sonucu.

Device bellek o konumu sunamıyor (yukarıdaki iki sebep,
[ADR 020](020-retype-semantigi.md)'nin M14 eki). Dolayısıyla
watermark kernel tarafında bir tabloya taşındı — ama **base adresle
anahtarlanarak**, çünkü nesneyi tanımlayan şey base adresi. Böylece
"bütün kopyalar aynı sayıyı görür" özelliği RAM'dekiyle birebir
korunuyor; yalnızca sayının durduğu yer değişiyor.

### Tablonun şekli

`untyped.rs`'de `DEVICE_UNTYPEDS`, `ioport::RANGES` deseniyle:
sabit boyutlu, `MAX_DEVICE_UNTYPED = 16` giriş, atomik alanlar.
Boyut keyfi değil: QEMU'nun 1280×800×32 ekranı 4 096 000 bayt ve
`2^k` bloklara **altı** parçada ayrılıyor (2 MiB + 1 + 512 KiB +
256 + 128 + 32). Bir ekran altı giriş demek; sekiz dar kalırdı.

Her girişte bir **referans sayacı** var, ve bu da kopyalanabilirliğin
sonucu: giriş, ondan hâlâ retype edebilecek son capability'den sonra
serbest kalmalı, önce değil. `irq::release_line` ve
`ioport::release_range` ilk silmede bırakır çünkü onların
adlandırdığı nesneyi tek bir capability adlandırır; bir Untyped
öyle değil.

Tohumlama tabloyu aşarsa **panic** — boot zamanında, deterministik,
ve mesajda kaç tane istendiği ile tavanın ne olduğu yazılı. Sessizce
durmak, ekranın bir bandının hiç değişmemesi olarak görünürdü ve
kimse onu tablo boyutuna kadar takip etmezdi.

## Paylaşım sınırı: B, A'nın çizdiğini görür

Bir device Untyped capability'si kopyalanabildiğine göre: iki bileşen
aynı framebuffer'ı eşleyebilir mi? **Evet.** Ve ikincisi, birincinin
çizdiği pikselleri okur.

**Tek sahip kuralı konmuyor**, ve bu ADR 041'in IRQ hattı ile
ADR 045'in port aralığı için verdiği karardan bilinçli bir ayrılma.

Sebep mekanizmada: `copy`, device Untyped için `retain_device`
çağırıyor — yani kopyalama **kasten destekleniyor** ve sayılıyor.
Tek sahip kuralı o mekanizmayı iptal ederdi.

Asıl sebep ise şu: **bir device Untyped'ı paylaşmak, cihazı
paylaşmaktır.** Kernel bunu engellemez çünkü engelleyemez. İki
bileşen aynı pikselleri görür, ve bu cihazın doğası — bir sızıntı
değil. Ekranın yanındaki kişi de aynı pikselleri görüyor.

Yetkiyi dağıtan bileşen (root task) kime verdiğine karar verir. Bu,
M13'ün IOPort'uyla aynı durum: 0x3F8'i iki bileşene vermek, iki
bileşeni aynı UART'a bağlar. Oradaki fark, kernel'in `Issue`'da
çakışmayı reddetmesiydi — ama o reddin konusu *yeni yetki üretmek*,
var olanı kopyalamak değil. ADR 045'in kendisi de `copy`'yi
engellemiyor.

Kısacası: **çakışma denetimi yetki üretme anındadır, dağıtma anında
değil.** Dağıtma, dağıtanın politikası.