# ADR 011 — Sayfa tablosu API'si: mekanizma, politika değil

Status: accepted (M3 architect pass)

## Bağlam

M3'te `map` / `unmap` / `translate` yazacağız. Bu API M11'de user VSpace'in
altına girecek — bir user bileşeni "şu Frame'i şu adrese eşle" dediğinde
çalışacak kod bu olacak. Dolayısıyla iki kısıt var:

**1. Politika içeremez.** Kernel "hangi adrese ne eşlenmeli" sorusuna cevap
vermez. Boş adres aramaz, otomatik genişletmez, swap etmez. Çağıran tam
olarak ne istediğini söyler; kernel ya yapar ya reddeder.

**2. Kaynak tüketimi sınırlı ve açık olmalı.** Bir eşleme ara sayfa tablosu
gerektirebilir ve o tablo bir çerçeve demektir. "Kernel kendi başına bellek
ayırmaz" kuralı burada en çok zorlanan yer: `map` bir PDPT/PD/PT yaratmak
zorunda kalabilir ve o çerçeve nereden gelecek?

M2'de `allocate()` global bir allocator'dan çerçeve veriyor. M5'te
capability modeli gelince bu Untyped retype'a dönüşecek. API'nin bugünkü
hali yarınki modeli engellememeli.

## Seçim

### Tipler

```rust
/// Sanal adres. PhysAddr'den ayrı bir tip: ikisini karıştırmak,
/// derleyicinin bedavaya yakalayabileceği bir hata sınıfı.
pub struct VirtAddr(u64);

/// 4 KiB hizalı sanal sayfa. Hizasız bir sayfa temsil edilemez.
pub struct Page(u64);          // sayfa indeksi, VirtAddr / 4096

/// Eşleme izinleri. bitflags.
pub struct PageFlags(u64);
```

`PageFlags` doğrudan donanım biti değil, soyut bir izin kümesi:

| Bayrak | Anlam |
|---|---|
| `READ` | Okunabilir. x86'da örtük (P biti), ama API'de açık olmalı |
| `WRITE` | Yazılabilir |
| `EXEC` | Yürütülebilir (donanımda NX'in yokluğu) |
| `USER` | CPL 3'ten erişilebilir |
| `GLOBAL` | CR3 değişiminde TLB'de kalır |
| `NO_CACHE` | Cache devre dışı (MMIO için) |
| `WRITE_THROUGH` | Write-through cache |

Donanım bitlerine çeviri API'nin içinde kalır. Sebep: x86'da `EXEC` bir
bitin *yokluğu* (NX set değil), `READ` ayrı bir bit değil. Çağıranın bu
tersliği bilmesi gerekmez, ve ileride başka bir mimariye taşınırsa arayüz
değişmez.

### İmzalar

```rust
pub unsafe fn map(
    table: &mut PageTable,
    page: Page,
    frame: PhysFrame,
    flags: PageFlags,
    allocator: &mut impl FrameSource,
) -> Result<MapGuard, MapError>;

pub unsafe fn unmap(
    table: &mut PageTable,
    page: Page,
) -> Result<(PhysFrame, UnmapGuard), UnmapError>;

pub fn translate(
    table: &PageTable,
    addr: VirtAddr,
) -> Result<(PhysAddr, PageFlags), TranslateError>;
```

Dört tasarım kararı:

**`FrameSource` bir trait.** `map` ara tablo için çerçeveye ihtiyaç
duyduğunda bunu çağırandan ister, global bir allocator'a gitmez. Bugün
M2'nin bitmap allocator'ı bu trait'i uygular; M5'te bir Untyped capability
uygular. Kernel'in "kendi adına allocate etmemesi" kuralı böyle korunuyor:
çerçeve her zaman çağıranın hesabından çıkar.

```rust
pub trait FrameSource {
    fn alloc_frame(&mut self) -> Option<PhysFrame>;
}
```

**Guard tipleri TLB disiplinini zorlar.** `map` ve `unmap`, kullanılması
zorunlu bir guard döndürür (`#[must_use]`). Guard, TLB geçersizleştirmesi
yapılana kadar düşürülemez. Ayrıntı [ADR 012](012-tlb-gecersizlestirme.md);
burada önemli olan, bunun tip sistemiyle zorlanması.

**`unmap` çerçeveyi döndürür, serbest bırakmaz.** Çerçevenin sahibi
çağırandır. Kernel'in onu serbest bırakması, kimin sahip olduğuna dair bir
politika kararı olurdu — ve capability modelinde çerçevenin sahibi
Untyped'ı tutan bileşendir, kernel değil.

**`map` zaten eşli bir sayfaya yazmaz.** Üzerine yazmak sessizce eski
eşlemeyi kaybetmek demek; o çerçeve artık kimsenin elinde olmayabilir —
bir leak. Değiştirmek isteyen önce `unmap` eder. Bu, "açıkça iste"
ilkesinin bir örneği.

### Hatalar

```rust
pub enum MapError {
    /// Sayfa zaten eşli. Üzerine yazmak eski çerçeveyi sessizce
    /// kaybetmek olurdu. Değiştirmek için önce unmap.
    AlreadyMapped { existing: PhysFrame },

    /// Ara sayfa tablosu için çerçeve yok. FrameSource None döndürdü.
    /// Olağan bir durum: bellek biter.
    OutOfFrames,

    /// Adres kanonik değil. #GP üretirdi, #PF değil -- kanoniklik
    /// kontrolü sayfa yürüyüşünden önce. Bkz. ADR 009.
    NotCanonical { addr: VirtAddr },

    /// USER bayrağı kernel yarısındaki bir adrese isteniyor ya da
    /// tersi. Sınır ihlali.
    WrongHalf { addr: VirtAddr, user: bool },

    /// Bayrak kombinasyonu geçersiz: hem WRITE hem EXEC (W^X ihlali),
    /// ya da hiçbir erişim biti yok.
    InvalidFlags(PageFlags),

    /// Eşlenmek istenen yolda büyük sayfa var. PMAP 2 MiB/1 GiB
    /// sayfalar kullanıyor; oraya 4 KiB eşleme yapılamaz.
    BlockedByHugePage { level: u8 },
}

pub enum UnmapError {
    /// Sayfa zaten eşli değil. Çift unmap, çift free ile aynı sınıf
    /// hata: sessizce kabul edilirse çerçeve iki kez serbest bırakılır.
    NotMapped,
    NotCanonical { addr: VirtAddr },
    BlockedByHugePage { level: u8 },
}

pub enum TranslateError {
    NotMapped,
    NotCanonical { addr: VirtAddr },
}
```

`Result` döndürmek, panic etmekten farklı bir karardır ve bilinçli: bu
hataların hepsi çağıranın yanlış bir şey istemesinden kaynaklanır, kernel'in
bozuk olmasından değil. M11'de bu çağıran bir user bileşeni olacak ve
kötü bir istek kerneli durdurmamalı — hata kodu dönmeli.

İstisna: `AlreadyMapped` ve `NotMapped` kernel'in kendi kodundan gelirse
bu bir kernel bug'ıdır. Kernel-içi çağrı yerleri `.expect()` ile
karşılayacak; user yolundan gelenler hata kodunu geri iletecek.

### Sayfa boyutu

`map` yalnızca 4 KiB. Büyük sayfalar PMAP kurulumunda ayrı bir fonksiyonla
(`map_range_huge`) yapılır, çünkü orası tek seferlik ve farklı kurallara
tabi. Genel `map`'in boyut parametresi almaması, çağıranın hizalama
hatası yapma ihtimalini ortadan kaldırıyor.

## Elenen alternatifler

**`map`'in global allocator'dan çerçeve çekmesi.** En kolay. Ama "kernel
kendi adına allocate etmez" kuralını ihlal eder ve M5'te capability
modeline geçerken her çağrı yerini değiştirmek gerekir. `FrameSource`
trait'i aynı kodu her iki dünyada çalıştırıyor.

**`map`'in boş adres bulması (`map_anywhere`).** Kolaylık fonksiyonu, ama
politika: "boş adres" tanımı çağırana aittir. User space kendi adres
uzayının yerleşimine karar verir.

**Hataları panic etmek.** Kernel-içi çağrılar için savunulabilir, user
yolundan gelenler için felaket: bir user bileşeni kerneli durdurabilirdi.

**Hata yerine `Option` dönmek.** Hangi hatanın olduğunu kaybeder.
`OutOfFrames` ile `AlreadyMapped` çağıran için tamamen farklı durumlar:
biri yeniden denenebilir, diğeri bir mantık hatası.

**`PageFlags` yerine doğrudan donanım bitleri.** Çağıran NX'in ters
mantığını ve PAT bitlerinin konumunu bilmek zorunda kalırdı. Soyut bayraklar
ayrıca geçersiz kombinasyonları (W+X) API sınırında yakalamayı mümkün
kılıyor.

**Guard tipleri yerine "TLB'yi unutmayın" yorumu.** M1'in `lateout`
hatası bunun neden yetmediğini gösterdi: kural kodda yazılı olmazsa
uygulanmaz. Bkz. [ADR 012](012-tlb-gecersizlestirme.md).

**`translate`'in yalnızca adres dönmesi.** Bayrakları da döndürmek, bir
eşlemenin izinlerini doğrulamayı mümkün kılıyor — hata ayıklama ve M11'de
capability doğrulaması için gerekli.

## Sonuç

- API mekanizma; politika yok. Adres seçimi, çerçeve sahipliği ve hata
  yorumu çağıranın işi.
- `FrameSource` trait'i sayesinde ara tablo çerçeveleri çağıranın
  hesabından çıkıyor. M5'te Untyped bu trait'i uygulayacak ve `map`'in
  kodu değişmeyecek.
- Yedi `MapError` varyantı, her biri ayrı bir yanlış kullanımı adlandırıyor.
  Hiçbiri sessizce yutulmuyor.
- `unmap` çerçeveyi iade ediyor, serbest bırakmıyor: sahiplik çağıranda.
- Zaten eşli bir sayfanın üzerine yazılamıyor — leak'in yapısal engeli.
- W^X API sınırında zorlanıyor: `WRITE | EXEC` bir hata.
- Guard tipleri TLB disiplinini tip sistemine bağlıyor.
- 4 KiB tek boyut; büyük sayfalar yalnızca PMAP kurulumunda.
- Açık nokta: `PageTable`'ın kendisi nasıl temsil edilecek (ham çerçeve mi,
  tiplenmiş bir yapı mı) ve M11'de bir VSpace capability'si ona nasıl
  bağlanacak. M5'te capability modeli şekillenince netleşir; API imzaları
  bundan etkilenmiyor.