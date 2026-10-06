# ADR 013 — BOOTLOADER_RECLAIMABLE geri kazanımı: ön koşullar doğrulanabilir kontrollere dönüşüyor

Status: accepted (M3 architect pass)

Devamı: [ADR 008](008-bootloader-reclaimable-dokunulmaz.md)

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

Geri kazanım `memory::reclaim_bootloader_memory()` içinde, ve fonksiyon
**dört kapıdan geçmeden** iş yapmıyor.

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

### Kapı 3 — Hiçbir canlı pointer reclaimable'a bakmıyor olmalı

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

### Kapı 4 — Serbest bırakma sayısı beklenenle uyuşmalı

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
geri kazanımın en zor ön koşulu buydu. Geri kalanı birkaç kopyalama ve dört
assert. Şimdi yapılmazsa ön koşullar bir daha bu kadar hazır olmayacak.

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

- ADR 008'in üç düzyazı ön koşulu, dört çalıştırılabilir kapıya dönüştü.
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
- M3 kabul testi şunu basacak: *"[mem] reclaimed N frames (M MiB) from
  bootloader-reclaimable; boot info verified after poisoning"*.