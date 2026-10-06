# ADR 013 — BOOTLOADER_RECLAIMABLE geri kazanımı: ön koşullar doğrulanabilir kontrollere dönüşüyor

Status: accepted (M3 architect pass), **amended (M3 implementation)**

Devamı: [ADR 008](008-bootloader-reclaimable-dokunulmaz.md)

> **M3 uygulama notu — dört kapı beşe çıktı.**
>
> Bu ADR dört kapı tanımladı. Uygulama beşinciyi — aslında sıra olarak
> üçüncüyü — ortaya çıkardı: **kernel, Limine'in yığınında duruyorken geri
> kazanım yapamaz.** Limine boot yığınını `BOOTLOADER_RECLAIMABLE`
> belleğinden ayırıyor, dolayısıyla zehirleme üstünde durduğumuz yığını
> siliyor.
>
> Belirti öğretici olduğu için ayrıntısıyla aşağıda, "Kapı 3" başlığında.
> Kısaca: `reclaim`'den dönen `ret`, dönüş adresi olarak
> `0xdededededededede` yükledi ve #GP aldı. Yani zehir deseni *tam olarak
> tasarlandığı gibi* çalıştı — sessiz bozulma yerine anında, kanonik
> olmayan bir adres. Kapıların en zayıfı sayılan zehirleme, kapıların
> listesindeki eksiği yakalayan şey oldu.
>
> Buradaki ders kapı sayısı değil: *tasarım "sayfa tablolarından in" dedi,
> ama "ayağının altındaki her şeyden in" demedi.* Sayfa tabloları ve yığın,
> üzerinde durulan iki ayrı şey, ve ikisini de bırakmak gerekiyor.

## Bağlam

ADR 008, reclaimable belleği (~4 MiB) M2'de dokunulmaz bıraktı ve geri
kazanım için üç ön koşul saydı:

1. Hâlâ ihtiyaç duyulan her şeyi kopyala.
2. Kendi sayfa tablolarımızı kur ve onlara geç.
3. Ancak o zaman çerçeveleri serbest bırak.

Bunlar doğru ama **düzyazı**. "Hâlâ ihtiyaç duyulan her şey" nedir? Biri
unutulursa ne olur? Cevap: bir süre hiçbir şey, sonra uzak bir yerde
açıklanamaz bir çöküş — çünkü `limine` crate'i response'ları `&'static`
döndürüyor ve Rust'ın bakış açısından o veri sonsuza kadar geçerli.
Derleyici bize yardım etmeyecek.

Bu ADR o üç maddeyi, atlanması durumunda **build'i ya da boot'u durduracak**
kontrollere çeviriyor.

## Hâlâ okunan Limine response'ları

Kodda geçen her istek ve M3 sonrası akıbeti:

| İstek | Ne okunuyor | M3 sonrası |
|---|---|---|
| `BootloaderInfoRequest` | `name`, `version` — `&str`, reclaimable'da | **Kopyala** (sabit boyutlu tampona) |
| `ExecutableAddressRequest` | `physical_base`, `virtual_base` — `u64` | Kopyala (zaten `AtomicU64`'te) |
| `HhdmRequest` | `offset` — `u64` | Kopyala (zaten `AtomicU64`'te) |
| `ExecutableCmdlineRequest` | `cmdline` — `&str`, reclaimable'da | **Kopyala** |
| `MemmapRequest` | `entries()` dizisi ve `Entry`'ler | **Kopyala** (en kritik) |
| `StackSizeRequest` | Yalnızca varlığı kontrol ediliyor | Kopyalamaya gerek yok |

İki sınıf var:

- **Skaler (`u64`)**: zaten kopyalanmış durumda, `AtomicU64` statiklerde
  duruyor. `hhdm_offset()` ve `kernel_virt_to_phys()` bunları okuyor,
  response'a dokunmuyorlar.
- **İşaretçi arkasındaki veri (`&str`, dizi)**: kopyalanmadı. Reclaimable
  serbest bırakılırsa bu pointer'lar dangling olur.

Üçüncü bir sınıf daha var ve en tehlikelisi: **Limine'in sayfa tabloları.**
Hiçbir response bunlara işaret etmiyor, ama CR3 onları gösteriyor. Bunları
"kopyalamak" değil, yerine kendi tablolarımızı koymak gerekiyor
([ADR 010](010-hhdm-kaderi.md)).

## Seçim

Geri kazanım `memory::bootinfo::reclaim()` içinde, ve fonksiyon
**beş kapıdan geçmeden** iş yapmıyor.

### Kapı 1 — Her response kopyalanmış olmalı

Kopyalanan veri statik, sabit boyutlu tamponlarda tutulur (heap M4'te):

```rust
struct BootInfo {
    bootloader_name: [u8; 64],
    bootloader_version: [u8; 64],
    cmdline: [u8; 256],
    memmap: [MemoryRegion; MAX_MEMMAP_ENTRIES],   // 128
    memmap_len: usize,
    hhdm_offset: u64,
    kernel_phys_base: u64,
    kernel_virt_base: u64,
}
```

Taşma sessizce kırpılmaz, **panic eder**: kırpılmış bir memory map, var
olmayan belleği dağıtmaya ya da var olanı kaybetmeye yol açar.

`MAX_MEMMAP_ENTRIES = 128` bir tahmin: bu makinede 16 girdi var, 128 bol
görünüyor. Ama bu bir varsayım ve panic mesajı bunu söyleyecek: *"memory map
has N entries, MAX_MEMMAP_ENTRIES is 128 — raise it"*. Doğrulanmamış bir
sayının sessizce yanlış davranmasındansa gürültülü başarısız olması
yeğdir.

### Kapı 2 — Kendi sayfa tablolarımızda çalışıyor olmalıyız

```rust
assert!(paging::using_own_tables(),
        "cannot reclaim bootloader memory while running on its page tables");
```

`using_own_tables()` CR3'ü okur ve M3'te kurduğumuz PML4'ün fiziksel
adresiyle karşılaştırır. Eşit değilse hâlâ Limine'in tablolarındayız ve
geri kazanım, altımızdaki eşlemeleri serbest bırakmak olurdu.

### Kapı 3 — Kendi yığınımızda çalışıyor olmalıyız

*(Bu kapı mimari geçişte yoktu; uygulama sırasında eklendi.)*

```rust
assert!(vm::on_own_stack(),
        "cannot reclaim bootloader memory while running on the bootloader's \
         stack: Limine allocates it from the very memory about to be \
         poisoned, so the next return would load the poison pattern");
```

Limine, kernel'e verdiği yığını `BOOTLOADER_RECLAIMABLE` bellekten ayırıyor.
Protokol bunu yasaklamıyor ve mantıklı da: o bellek zaten bootloader'ın.
Test makinesinde yığın fiziksel `0x0ff6e000`'de, yani
`0x0ff45000..0x0ffdf000` reclaimable bölgesinin ortasında.

Sonuç: Kapı 4'ün zehirlemesi, **üzerinde durduğumuz yığını** `0xDE` ile
dolduruyor. `reclaim` kendi `ret`'ine geldiğinde yığından dönüş adresi
olarak `0xdededededededede` çekiyor.

Gözlenen çıktı:

```
*** EXCEPTION: #GP general protection (vector 13) ***
[trap] error_code=0x0000000000000000  rip=0xdededededededede  cs=0x0008
[trap] rflags=0x0000000000000046  rsp=0xffff80000ff6ee30  ss=0x0010
```

Teşhisi veren `rsp`: `0xffff80000ff6ee30`, yani PMAP üzerinden fiziksel
`0x0ff6ee30` — memory map'teki reclaimable girdinin tam içi.

**Çözüm.** Kernel, geri kazanımdan önce kendi yığınına geçiyor:
`vm::init` KSTACK slot 1'e 16 KiB'lık bir yığın (altında guard page ile)
eşliyor, `vm::switch_to_main_stack` RSP'yi oraya alıp `kmain`'e **atlıyor**.
Atlamak zorunlu, çağırmak değil: RSP değiştiği anda eski yığındaki yerel
değişkenler, saklı kayıtlar ve dönüş adresi birden erişilemez oluyor, yani
fonksiyon dönemez. `kmain`'in imzası `extern "C" fn() -> !`.

Bu, `kstart`/`kmain` ayrımının *sebebi*. Estetik bir bölme değil: iki
fonksiyon iki ayrı yığında çalışıyor ve aradaki sınır geçilemez. Bir yan
fayda, M3'te asıl hatayı bulmadan önce yanlışlıkla "düzelttiğim" şeyi
kalıcı olarak engelliyor olması — `kstart`'ta okunan `cmdline`,
reclaimable belleğe bakan bir `&'static str`'di ve `kmain`'den
erişilemiyor, çünkü onunla birlikte eski yığın da gitti. Hatırlamaya değil
kapsama dayanan bir garanti.

### Kapı 4 — Hiçbir canlı pointer reclaimable'a bakmıyor olmalı

Bu, statik olarak kanıtlanamaz. Yapılabilecek olan: geri kazanımdan **önce**
reclaimable bölgeleri bir zehir deseni ile doldurmak, sonra kernel'in kendi
tutarlılık kontrollerini çalıştırmak.

```rust
// Serbest bırakmadan önce 0xDE ile doldur.
// Birisi hâlâ okuyorsa, "DEDEDEDE" çöpü sessiz bozulmadan daha görünür --
// ve bir pointer olarak kullanılırsa kanonik olmayan bir adres, yani #GP.
poison_reclaimable_regions(0xDE);
verify_boot_info_still_consistent();   // kopyalanan veriyi tekrar oku
```

Zayıf bir kontrol, ve öyle olduğunu kabul ediyorum: yalnızca geri kazanım
anında canlı olan bir okumayı yakalar. Ama maliyeti düşük ve M3'ün kabul
testinde "zehirlemeden sonra kernel hâlâ doğru çalışıyor" iddiası
denetlenebilir hale geliyor.

`0xDE` seçimi bilinçli: `0xDEDEDEDE_DEDEDEDE` kanonik olmayan bir adres
(bit 47 set, üst bitler farklı), yani pointer olarak kullanılırsa #GP
üretir — sessizce geçerli bir adrese dönüşmez.

### Kapı 5 — Serbest bırakma sayısı beklenenle uyuşmalı

```rust
let reclaimed = release_reclaimable_frames();
assert_eq!(reclaimed, expected_reclaimable_frames,
           "reclaimed {} frames but the memory map said {}",
           reclaimed, expected_reclaimable_frames);
```

`expected_reclaimable_frames` kopyalanan memory map'ten hesaplanır.
Uyuşmazlık, ya bir bölgenin atlandığı ya da fazladan bir şeyin serbest
bırakıldığı anlamına gelir — ikisi de ciddi.

### ACPI_RECLAIMABLE kapsam dışı

ACPI tabloları hiç okunmadı; M6/M7'de cihaz keşfi için gerekecekler.
`is_allocatable()` onları dışarıda bırakmaya devam edecek. Bu, ADR 008'in
sonucunun tekrarı ve M3'te değişmiyor.

## Elenen alternatifler

**Geri kazanımı M3'te de ertelemek.** 4 MiB, 256 MiB'lık bir makinede %1,6.
Erteleme savunulabilirdi — ama M3 sayfa tablosu geçişini zaten yapıyor, ki
geri kazanımın en zor ön koşulu buydu. Geri kalanı birkaç kopyalama ve
birkaç assert. Şimdi yapılmazsa ön koşullar bir daha bu kadar hazır
olmayacak.

**Yığın değiştirmek yerine Limine'in yığınının bulunduğu bölgeyi geri
kazanımdan muaf tutmak.** Yani: o bir bölgeyi atla, gerisini al. Daha az
kod. Üç nedenle reddedildi. (a) Hangi bölgenin yığını içerdiğini RSP'ye
bakarak bulmak gerekir — çalışma zamanında keşfedilen, protokolde garanti
edilmeyen bir ilişki. (b) Kernel sonsuza kadar bootloader'ın ayırdığı bir
yığında kalır; boyutunu biz seçmemiş oluruz ve altında guard page olmaz.
(c) M5'te thread'ler gelince her thread'in kendi yığını olacak, yani bu iş
zaten yapılacak — erteleme, iptal değil.

**Kopyalama yerine reclaimable'ı kalıcı eşlemek ve okumaya devam etmek.**
Yani: bellek olarak geri kazanma, ama eşlemeyi koru. Hiçbir şey kazandırmaz
— amaç zaten o belleği kullanmak.

**Rust'ın ömür sistemiyle çözmek.** Response'lar `&'static` ve crate'in
API'si bu; `'static`'i daraltmak için crate'i fork etmek gerekir. Kazanç
gerçek olurdu (derleyici dangling pointer'ı yakalardı), ama bağımlılığı
çatallamanın bedeli ağır. Not olarak duruyor: bir gün `limine` crate'ine
ömürlü bir API önerilebilir.

**Zehirlemeyi atlamak.** Dört kapıdan en zayıfı ve tek "koşu zamanı
sezgisi" olan bu. Ama bedeli bir `memset` ve birkaç yeniden okuma;
yakaladığı hata sınıfı ise en pahalısı.

## Sonuç

- ADR 008'in üç düzyazı ön koşulu, beş çalıştırılabilir kapıya dönüştü.
  (Mimari geçişte dört sayılmıştı; beşincisi uygulamada bulundu.)
- Kernel, geri kazanımdan önce kendi yığınına geçiyor. Limine'in yığını
  reclaimable bellekte ve zehirleme onu siliyor. `kstart`/`kmain` ayrımı
  bu yüzden var.
- Kopyalanacak veri listelendi: iki `&str`, memory map, üç skaler.
  Skalerler zaten kopyalanmış durumda.
- Sabit boyutlu tamponlar taşmada panic eder; sessiz kırpma yok.
- `MAX_MEMMAP_ENTRIES = 128` doğrulanmamış bir tahmin ve panic mesajı bunu
  söylüyor.
- Zehirleme deseni `0xDE`: kanonik olmayan bir adres ürettiği için pointer
  olarak kullanılırsa #GP verir.
- Beklenen/gerçek çerçeve sayısı karşılaştırması, sessiz bir atlamayı
  yakalar.
- `ACPI_RECLAIMABLE` kapsam dışı; M6/M7'ye kadar dokunulmaz.
- M3 kabul testinin gerçek çıktısı:

  ```
  [vm] switching stack: 0xffff80000ff6edc0 -> 0xffffc80000009ff8 (off the
       bootloader's stack, which reclaim is about to poison)
  [boot] now on the kernel's own stack
  [mem] poisoned 4504 KiB of reclaimable memory with 0xde; boot info still
        reads back correctly
  [mem] reclaimed 1126 frames (4504 KiB) from bootloader-reclaimable
  [boot] after reclaim: bootloader still reported as Limine 9.6.7
  ```

  `run.py` bu satırların hepsini **her** senaryoda arıyor, ve ayrıca
  `0xdededededededede`'yi normal boot'ta yasaklıyor: zehir deseninin bir
  kayıt dökümünde görünmesi, bir şeyin işaret ettiği belleğe göre daha
  uzun yaşadığı anlamına gelir.