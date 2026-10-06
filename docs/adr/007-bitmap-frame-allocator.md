# ADR 007 — Fiziksel çerçeve ayırıcı olarak bitmap

Status: accepted (M2)

## Bağlam

Kernel'in 4 KiB'lık fiziksel çerçeveleri dağıtıp geri alabilmesi gerekiyor.
M2'nin kısıtları seçimi daraltıyor:

- **`alloc` yok.** Heap M4'ün işi. Allocator'ın metadata'sı ya statik olmalı
  ya da yönettiği bellekten kendini bootstrap etmeli.
- **Kernel boot sonrası kendi adına allocate etmez.** Metadata bir kerelik,
  başlangıçta ayrılmalı.
- **Hiçbir kernel yolunda sınırsız döngü olmaz.** Arama bir üst sınırla
  bitmeli.
- **Çift serbest bırakma tespit edilmeli.** Sessizce kabul edilirse aynı
  çerçeve iki sahibe dağıtılır ve bozulma, sebebinden çok uzakta görünür.

256 MiB'lık bir makinede ~64 000 çerçeve var; daha büyük makinelerde
milyonlarca.

## Seçim

**Tüm fiziksel bellek üzerinde bir bitmap.** Çerçeve başına bir bit; set =
kullanımda veya asla dağıtılmaz, temiz = boş.

Üç ayrıntı seçimi tanımlıyor:

**1. "Her şey dolu"dan başla.** Bitmap `u64::MAX` ile doldurulur, sonra
yalnızca Limine'in USABLE dediği bölgeler temizlenir. Böylece memory map'in
hiç bahsetmediği adres aralıkları — delikler, MMIO, firmware — ayrı bir
kontrol gerektirmeden dışarıda kalır. Ters sırada (her şey boş, sonra
rezerve et) unutulan bir bölge sessizce dağıtılabilir hale gelirdi.

**2. Bitmap, yönettiği belleğin içinde.** Başlangıçta yeterince büyük ilk
USABLE bölge bulunur, bitmap oraya konur, sonra o çerçeveler kullanımda
işaretlenir. 64 000 çerçeve için 7 KiB; 1 TiB RAM için 32 MiB. Statik bir
dizi, önceden boot edebileceğimiz en büyük makineye göre boyutlandırılmak
zorunda kalırdı ve bu alan her küçük makinede imajda boşa giderdi.

**3. Arama iki geçişli ve sınırlı.** Son bulunan çerçeveden sonuna, sonra
baştan oraya. `u64::MAX` olan kelimeler tek karşılaştırmayla atlanır;
`trailing_ones()` ilk temiz biti doğrudan verir. İpucu yalnızca bir
optimizasyon — yanlış olması doğruluğu etkilemez.

Çift serbest bırakma `free()` içinde yakalanır: bit zaten temizse panic.

## Elenen alternatifler

**Serbest liste (free list).** Boş çerçevelerin kendi içine bir `next`
pointer'ı yazılır; metadata sıfır ek bellek, alloc/free O(1). Cazip, ama:

- **Çift serbest bırakma tespit edilemez.** Listeye aynı çerçeveyi iki kez
  eklemek O(1)'de fark edilemez; anlamak için listeyi taramak gerekir, o da
  O(n). Bitmap'te aynı kontrol bir bit okuması. Bu milestone'un negatif
  testi tam olarak bu; tespit edemeyen bir yapı kabul testini geçemez.
- **Boş çerçevelerin içeriği yazılabilir olmalı.** Bu doğru, ama her free
  bir sayfaya dokunmak demek — cache kirliliği ve ileride (M3'te) o
  sayfanın eşlenmiş olmasını gerektiriyor.
- **Bitişik çerçeve aramak imkânsız.** M3'te sayfa tabloları için ardışık
  çerçeve gerekebilir; bitmap'te bu bir pencere taraması, listede bir
  yeniden tasarım.

**Buddy allocator.** Bitişik blokları verimli yönetir ve parçalanmayı
sınırlar. Ama M2'nin ihtiyacı tek çerçeve; buddy'nin karmaşıklığı (seviye
başına liste, bölme/birleştirme) karşılığında şu an hiçbir şey kazandırmıyor.
Bitmap'in üstüne sonradan eklenebilir — tersi doğru değil.

**Stack of free frames (dizi).** Basit ve O(1), ama dizinin kendisi
çerçeve sayısı kadar `u64` yer kaplar: bitmap'in 64 katı.

**`alloc` + `Vec`.** Yasak; ayrıca tavuk-yumurta: heap'in kendisi
çerçevelere ihtiyaç duyar.

## Sonuç

- Çerçeve başına 1 bit: 256 MiB için 7 KiB metadata (%0,003).
- `allocate()` tükendiğinde `None` döner, panic etmez. Bellek bitmesi
  olağan bir durumdur; hiçbir kernel yolu ayırmanın başarılı olacağını
  varsaymaz.
- `free()` çift serbest bırakmada panic eder ve çerçeve adresini basar.
- Çerçeve 0 ve kernel imajı açıkça rezerve edilir. İmaj için ek bir kontrol
  var: Limine zaten `EXECUTABLE_AND_MODULES` işaretlediği için `reserve_range`
  genelde 0 bit değiştirir, bu da "rezerve etmeyi unuttuk" ile aynı görünür.
  Bu yüzden sayıya değil, özelliğin kendisine assert ediliyor.
- **Kilitleme yok.** Tek CPU, kesmeler kapalı. Diğer CPU'lar geldiğinde
  gerçek bir kilit şart; `unsafe impl Sync` bunu açıkça geçici olarak
  işaretliyor. Daha önemlisi: M5'te capability modeli gelince bu allocator
  doğrudan çağrılabilir bir servis olmaktan çıkıp Untyped'ın arkasındaki
  uygulama haline gelecek.
- Ardışık çerçeve ayırma henüz yok; bitmap bunu mümkün kılıyor, M3
  gerektirdiğinde eklenecek.