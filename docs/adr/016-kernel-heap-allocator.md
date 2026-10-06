# ADR 016 — Kernel heap: boyut sınıflı serbest listeler

Status: accepted (M4), **ölçümlerle güncellendi (M4 uygulama)**

İlgili: [ADR 011](011-sayfa-tablosu-api.md) (sayfalar `map()` ile eşlenir),
[ADR 007](007-bitmap-frame-allocator.md) (çerçeveler oradan gelir)

## Bağlam

M4, `alloc` crate'ini açıyor: `Box`, `Vec`, `BTreeMap`, `String`. Bunun
için bir `#[global_allocator]` gerekiyor ve seçimi belirleyen tek bir
kısıt var.

**M9'da IPC yolunda allocate edilecek.** Master prompt'un kuralı net:
kernel yolunda sınırsız döngü yok. Bir IPC çağrısının süresi, heap'in
geçmişine — kaç blok serbest, nasıl parçalanmış — bağlı olamaz. Yani
`alloc` ve `dealloc`'un **en kötü durumu bir sabitle sınırlı** olmalı,
"genellikle hızlı" değil.

Bu, aday tasarımların çoğunu eliyor.

## Adaylar ve en kötü durumları

| Tasarım | alloc en kötü | dealloc en kötü | Karar |
|---|---|---|---|
| Serbest liste (first-fit) | O(serbest blok sayısı) — **sınırsız** | O(1) ekleme, O(n) birleştirme | Elendi |
| Serbest liste (best-fit) | O(n), her zaman tam tarama | O(n) | Elendi |
| Buddy | O(log(max/min)) = 17 seviye | O(log) birleştirme | Değerlendirildi |
| Boyut sınıflı serbest listeler | **O(1)**, döngüsüz | **O(1)**, döngüsüz | **Seçildi** |

Serbest liste varyantlarının sorunu "yavaş" olmaları değil — parçalanmış
bir heap'te tarama uzunluğunun bir üst sınırı olmaması. Bir IPC çağrısının
en kötü durumu hesaplanamıyorsa, gerçek zamanlı bir iddia da kurulamaz.

Buddy savunulabilirdi: 16 B ile 1 MiB arasında 17 mertebe, yani alloc'ta
en fazla 17 adım yukarı arama + 17 bölme, dealloc'ta en fazla 17
birleştirme. Sınırlı ve parçalanma davranışı iyi bilinen. İki nedenle
seçilmedi:

1. **Bookkeeping belleği.** Buddy'nin hangi eşin serbest olduğunu bilmesi
   gerekir. Mertebe başına bit haritası, 16 MiB heap ve 16 B taban blok
   için ~256 KiB. Bunu statik ayırmak imajı şişirir; dinamik ayırmak
   tavuk-yumurta.
2. **Sabit 17, sabit 1'den kötü.** Birleştirme buddy'nin asıl faydası —
   parçalanmayı geri alır. Ama o faydanın bedeli her dealloc'ta bir döngü,
   ve aşağıda anlatıldığı gibi birleştirmeden de yaşanabilir.

## Seçim: boyut sınıflı serbest listeler, birleştirme yok

17 boyut sınıfı, ikinin kuvvetleri: 16 B, 32 B, ..., 1 MiB.

Her sınıfın kendi serbest listesi var ve liste **bloğun içinde** duruyor:
serbest bir blok ilk 8 baytında bir sonraki serbest bloğun adresini
tutuyor. Yani yan bellek sıfır — sınıf başına bir baş işaretçisi dışında
(17 × 8 = 136 bayt statik).

```
alloc(layout):
    class = class_index(layout)          // 3 komut, döngüsüz
    if head[class] != null:
        block = head[class]
        head[class] = *block             // 3 bellek işlemi
        return block
    else:
        return carve_from_frontier(class)   // yavaş yol

dealloc(ptr, layout):
    class = class_index(layout)
    *ptr = head[class]
    head[class] = ptr                    // döngü YOK
```

### En kötü durum, sayıyla

**`dealloc`: koşulsuz O(1).** Hiçbir döngü, hiçbir dal yok — sınıf
indeksi hesabı (`next_power_of_two().trailing_zeros()`, tek bir `lzcnt`)
artı iki bellek yazması. **10 komutun altında, her zaman.**

**`alloc` hızlı yol: O(1).** Sınıf indeksi + listeden çıkarma.
**15 komutun altında, döngüsüz.**

**`alloc` yavaş yol** (sınıfın listesi boş): frontier'ı sınıf boyutu kadar
ilerlet. Bu da döngüsüz — blokları önceden parçalamıyoruz, her seferinde
bir blok veriyoruz. Tek sınırsız görünen şey, frontier eşlenmemiş bölgeye
girdiğinde sayfa eşlemek:

- Eşlenecek sayfa sayısı: `ceil(sınıf_boyutu / 4096)`, en fazla **256**
  (1 MiB sınıfı için).
- Her eşleme: 4 seviyeli yürüyüş, en fazla 3 ara tablo ayırma.
- Yani yavaş yolun en kötüsü: **256 × (4 yürüyüş + 3 çerçeve ayırma)**.

Bu sabit ama küçük değil. **M9'un IPC yolu için çözüm:** kullanılan
sınıfları önceden ısıt. IPC yalnızca birkaç sabit boyutlu yapı ayıracak;
boot sırasında o sınıflara yeterince blok koyarsak yavaş yol IPC sırasında
hiç çalışmaz ve kalan tek maliyet 15 komutluk hızlı yol olur. Bunu M9'un
görevi olarak işaretliyorum — `heap::prewarm(class, count)` API'si M4'te
yazılmadı ama yeri belli.

### Ölçüm: yavaş yol tahmin ettiğimden çok daha sık çalışıyor

Yukarıdaki paragrafı yazarken yavaş yolun "sınıf başına bir kez, sonra
hiç" çalışacağını varsaymıştım. **Kabul testi bunu yanlışladı:**

```
[heaptest] calls: 10101 alloc, 10101 dealloc, 2753 grow
```

10101 ayırmanın **2753'ü** (%27) heap'i büyüttü. Nedeni, geriye dönüp
bakınca açık ve tasarımın doğrudan sonucu: serbest listeler ancak *bir
şey serbest bırakıldığında* dolar. Stres fazı 10000 bloğu aynı anda canlı
tutuyor, yani hiçbir şey geri gelmiyor, yani her ayırma frontier'dan
kesiyor ve frontier düzenli olarak eşlenmemiş alana giriyor.

Bu bir hata değil — allocator doğru çalışıyor — ama "yavaş yol nadirdir"
cümlesi yanlıştı. Doğrusu: **yavaş yolun sıklığı iş yüküne bağlı.**
Serbest bırakma oranı yüksek bir iş yükünde (kernel'in beklenen deseni)
listeler dolu kalır ve yavaş yol gerçekten nadir olur. Monoton büyüyen bir
iş yükünde her ayırma yavaş yoldan geçebilir.

M9 için sonuç değişmiyor ama gerekçesi güçleniyor: ön-ısıtma bir
optimizasyon değil, **IPC'nin sınırlı kalması için gereklilik.** Isıtma
olmadan IPC yolundaki bir ayırma, listenin o an boş olmasına bağlı olarak
15 komut ya da 256 sayfa eşleme sürer — ve hangisi olacağı önceden
bilinemez.

### Ölçüm: parçalanma ve eşli kalan sayfalar

Kabul testinin raporladıkları:

```
[heaptest] fragmentation: 5925046 bytes requested, 9367936 bytes reserved
           (158% -- the size classes guarantee at most 200%)
[heaptest] baseline: 0 bytes in use (was 0), 3071 pages still mapped
[heaptest] free lists: 10003 blocks across 12 of 17 classes
           (9668 KiB held for reuse)
```

Üç şey söylüyor:

**İç parçalanma %158.** Tahmin edilen en kötü %200'ün altında, ve bu
rakam ikinin kuvveti sınıfların gerçek maliyeti. Rastgele boyutlar için
teorik ortalama ~%133; test odaklı sınıf başına eşit dağılım kullandığı
için biraz daha yüksek.

**Her şey serbest bırakıldıktan sonra 12 MiB eşli kalıyor.** Canlı bayt
sayısı sıfıra döndü ama heap küçülmedi: 9668 KiB serbest listelerde,
gerisi iç parçalanma ve hizalama boşlukları. Bu, "sınıf başına tepe
noktalarının toplamı" iddiasının somut hâli — ve tasarım gereği, kusur
değil. Yine de **raporlanıyor**, çünkü sessizce tutulan 12 MiB ile
sızdırılan 12 MiB arasındaki fark yalnızca belgelenmiş olmaktır.

**17 sınıftan 12'si kullanıldı.** Büyük sınıflar (128 KiB ve üstü) stres
fazında hiç dokunulmadı; onlara yalnızca `check_growth` ve `run_oom`
giriyor.

### Birleştirmemenin bedeli: parçalanma

Serbest bırakılan 64 KiB'lık bir blok asla 16 baytlık bir isteğe
hizmet edemez. Sonuç: en yüksek bellek kullanımı, *toplam* tepe noktası
değil, **sınıf başına tepe noktalarının toplamı**.

Bir uygulama 1000 × 16 B ayırıp serbest bırakıp sonra 1000 × 32 B ayırsa,
heap her ikisini de tutar: 48 KB yerine 16 KB. Patolojik durumda, her
sınıftan tepe kullanımı farklı zamanlarda olursa, israf 17 katına kadar
çıkabilir.

**Neden kabul edilebilir.** Kernel'in ayırma deseni bir uygulamanınki gibi
değil: az sayıda nesne tipi, tekrar eden boyutlar, uzun ömürler. TCB'ler,
capability slot'ları, IPC tamponları — her biri kendi sınıfında yaşayıp
ölecek. Tam da bu desen için sınıf başına tepe ≈ toplam tepe.

**İç parçalanma** ayrı bir maliyet: 17 bayt isteyen 32 alır. İkinin
kuvveti sınıflarda en kötü durum 2×, rastgele boyutlarda ortalama ~1,3×.
Ölçülüyor ve raporlanıyor (`heap: N bytes in use, M bytes allocated`).

**Ölçülmemiş.** Gerçek parçalanma davranışı ancak gerçek bir iş yüküyle
görülür ve M4'te öyle bir yük yok. M9'dan sonra bu ADR yeniden ele
alınmalı: eğer sınıf başına tepe toplamı sorun olursa, çözüm tamamen
boş sayfaları sınıftan geri alıp frontier'a iade etmek olur — ki bu, her
sayfanın kaç canlı bloğu olduğunu saymayı gerektirir.

### Maksimum 1 MiB

1 MiB'dan büyük tek bir ayırma **panic ediyor**:

```
heap allocation of 2097152 bytes (align 8) exceeds the largest size class
(1048576); the kernel heap is for small objects -- large contiguous
regions should come from the frame allocator directly
```

Alternatif, büyük istekleri doğrudan sayfa aralığı olarak karşılamaktı.
Ama o zaman serbest bırakılan sayfa aralıklarının yeniden kullanımı için
bitişik aralık araması gerekir — tam da elemek istediğimiz sınırsız arama.
Kernel'in 1 MiB'dan büyük tek bir heap nesnesine ihtiyacı olduğu an,
doğru cevap muhtemelen heap değil zaten.

## Hizalama

Bloklar frontier'dan sınıf boyutuna hizalı kesiliyor, ve sınıf boyutları
ikinin kuvveti — yani 2^k boyutlu bir blok 2^k hizalı. `Layout` daha
büyük bir hizalama isterse sınıf ona göre yukarı yuvarlanıyor:

```rust
let needed = layout.size().max(layout.align()).max(MIN_BLOCK);
```

Bu, `align = 4096` isteyen bir 64 baytlık ayırmanın 4096 baytlık sınıftan
geleceği anlamına geliyor. İsrafı var ama doğru, ve kernel'de bu istek
nadir.

## Heap'in beslendiği kaynak — M5'te değişecek yer

Heap, sayfaları [ADR 011](011-sayfa-tablosu-api.md)'in `map()`'i ile
eşliyor ve çerçeveleri M2'nin allocator'ından alıyor. "Kernel kendi adına
allocate etmez" kuralı burada **bilinçli olarak** esnetiliyor.

Bu bir ihlal değil, ertelenmiş bir bağlantı. Kaynak tek bir yerde
toplandı:

```rust
/// The single point where the heap takes physical memory.
///
/// M5 REPLACES THIS. When Untyped capabilities exist, the heap will be
/// backed by a FrameSource handed to it at init, and this function will
/// become `self.source.alloc_frame()`. The signature is already the one
/// FrameSource uses, so the change is local to this function.
fn heap_frame_source() -> Option<PhysFrame>
```

`grep -n "M5 REPLACES THIS" kernel/src/` ile bulunabilir. M5'te buraya
bir `FrameSource` bağlanacak ve heap, kendi bütçesini bir capability'den
alan sıradan bir tüketici olacak.

## Elenen alternatifler

**Hazır bir crate (`linked_list_allocator`, `buddy_system_allocator`).**
Çalışırdı ve kod yazmaktan kurtarırdı. İki sorun: `linked_list_allocator`
tam olarak elediğimiz sınırsız aramayı yapıyor; ve bu projede her `unsafe`
bloğun gerekçesini biz yazıyoruz, bir bağımlılığın içindeki `unsafe`'i
denetleyemeyiz. Ayrıca M5'te allocator'ı bir capability'ye bağlamak,
içini değiştirmeyi gerektirecek.

**Slab allocator (tip başına önbellek).** Boyut sınıflarının tip-farkında
versiyonu; nesne başına kurucu/yıkıcı önbellekleme de yapar. M5'te
capability nesneleri geldiğinde cazip olacak, ama M4'te hangi tiplerin
sıcak olduğunu bilmiyoruz ve tip-başına yapı, tip listesi olmadan
yazılamaz. Boyut sınıfları onun basit hâli ve üzerine slab eklenebilir.

**Bump allocator, serbest bırakma yok.** En basit ve en hızlı. `Vec`'in
yeniden tahsisi bile belleği geri vermediği için kernel ömrü boyunca
monoton büyür. Bir boot-zamanı arena'sı için doğru olurdu; genel
`#[global_allocator]` için değil.

**Heap'i sabit boyutlu statik bir dizi yapmak.** `.bss`'te 16 MiB. Her
makinede o kadar imaj, ve 256 MiB'lık bir makinede heap'in %6'sı boşa.
Talep üzerine büyümek M3'ün `map()`'i varken ucuz.

## Uygulama sırasında öğrenilen: LLVM ayırmaları eleyebilir

`heap-oom` senaryosu ilk çalıştırıldığında şunu bastı:

```
[PANIC] internal error: entered unreachable code:
        the heap handed out 72 MiB without hitting its 64 MiB limit
```

Yani heap, 64 MiB sınırını aşmış görünüyordu. Sınır kontrolü doğruydu;
**ayırmalar hiç olmamıştı.** LLVM `alloc`'u bir ayırma fonksiyonu olarak
tanıyor ve sonucu kullanılmayan bir ayırmayı silmekte serbest. Testin tek
yaptığı null kontrolüydü, ki bu "kullanım" sayılmıyor.

Düzeltme, her bloğun iki ucuna `write_volatile` koymak. Aynı tuzak M3'te
`vmtest::recurse`'ü de yakalamıştı: kullanılmayan bir yığın dizisi
silinmiş ve "yığın taşması" testi hiçbir şey taşırmamıştı.

Genel ders, bu projede ikinci kez karşılaşıldığı için yazmaya değer:
**etkisi optimize edilip silinebilen bir test, test değildir.** Kaynak
tüketen negatif testlerde tüketimin gözlemlenebilir olması gerekiyor —
`volatile` ya da `black_box` ile.

## Sonuç

- 17 boyut sınıfı (16 B .. 1 MiB), sınıf başına intrusive serbest liste.
- `dealloc` koşulsuz O(1), **döngüsüz**, 10 komutun altında.
- `alloc` hızlı yol O(1), döngüsüz, 15 komutun altında. Yavaş yol en fazla
  256 sayfa eşleme.
- **Yavaş yolun sıklığı iş yüküne bağlı ve tahminimden yüksek çıktı:**
  kabul testinde 10101 ayırmanın 2753'ü büyüme tetikledi, çünkü hiçbir şey
  serbest bırakılmadan 10000 blok canlı tutuldu. M9'da ön-ısıtma bu yüzden
  bir optimizasyon değil, IPC'nin sınırlı kalması için gereklilik.
- Birleştirme yok: parçalanma sınıf başına izole. Ölçüldü — her şey
  serbest bırakıldıktan sonra 12 MiB eşli kalıyor, 9.4 MiB'ı serbest
  listelerde. Raporlanıyor.
- İç parçalanma ölçüldü: %158, teorik en kötü %200'ün altında.
- 1 MiB üstü tek ayırma panic eder, mesajda boyut ve hizalama ile.
- Çerçeve kaynağı tek bir fonksiyonda; M5'te `FrameSource`'a bağlanacak,
  yeri kodda `M5 REPLACES THIS` ile işaretli.