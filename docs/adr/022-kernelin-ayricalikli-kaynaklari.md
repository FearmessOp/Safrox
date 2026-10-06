# ADR 022 — Kernel'in ayrıcalıklı kaynakları: kilit, ve heap'in bütçesizliği

Status: accepted (M5 architect pass)

M4'ten devreden iki maddeyi karara bağlıyor:
`unsafe impl Sync for FrameAllocator` ve `heap_frame_source()`.

## Bağlam

Capability modelinin iddiası: **kernel kendi adına bellek ayırmaz.** Her
tahsis bir bileşenin Untyped'ından çıkar, yani bir bileşen diğerinin
belleğini tüketemez ve kernel'in sınırsız büyüyen bir havuzu yoktur.

M2 ve M4'te bu iddia iki yerde çiğneniyor:

1. `frame_allocator` global bir bitmap üzerinden çerçeve dağıtıyor,
   kimsenin bütçesinden düşmeden.
2. `heap` o allocator'dan sayfa alıp büyüyor — `heap_frame_source()`
   fonksiyonu, kodda `M5 REPLACES THIS` diye işaretli.

Her ikisi de M5'te karara bağlanmalı. Ayrıca bir eşzamanlılık sorunu
var: `unsafe impl Sync for FrameAllocator` bugün doğru, ama M6'da
kesmeler açıldığında yanlış olabilir.

## Bölüm A — Allocator'ın yeri ve kilit

### Bugünkü gerekçe ve ne zaman çöküyor

M2'nin `unsafe impl Sync` yorumu şunu diyor: tek CPU, kesmeler kapalı,
her giriş noktası yeniden-girişsiz tek bir `with()` fonksiyonundan
geçiyor.

**M6'da kesmeler açılacak** ve bu gerekçenin ikinci maddesi düşecek. O
andan itibaren bir kesme handler'ı `allocate()` çağırırsa — kernel
`with()`'in ortasındayken — bitmap bozulur. Tek CPU'da bile.

Bu, M6'nın yapacağı bir hata değil; M6'nın *mümkün kılacağı* bir hata.
Bugün kimse yapmıyor, yarın biri yapabilir, ve belirtisi rastgele bellek
bozulması olur.

### Karar: iki katmanlı

**1. Kesme handler'ları allocate etmez. Bu bir kural, ve denetlenecek.**

Kernel'in kesme yolunda tahsis yapmaması zaten doğru tasarım: bir kesme
handler'ı mümkün olduğunca kısa olmalı ve "bellek yok" durumunda ne
yapacağı belirsizdir. M12'de bir cihaz kesmesi geldiğinde yapılacak tek
şey bir Notification'ı sinyallemek olacak — ki o, önceden ayrılmış bir
nesneye bir bit yazmak.

Denetim: `frame_allocator::allocate()` ve `heap`'in `alloc`'u,
**kesme bağlamında olup olmadığını kontrol edip panic edecek.** Bir
sayaç (`interrupt_depth`) kesme girişinde artar, çıkışında azalır;
sıfır değilse tahsis bir kernel hatasıdır.

Bu, kilit değil **tripwire** — M4'teki `IN_ALLOC` bayrağıyla aynı
felsefe. Kuralı koda yazıp ihlalini gürültülü yapıyor.

**2. Gerçek kilit SMP ile gelir, şimdi değil.**

Tek CPU'da, kesme handler'ları tahsis yapmıyorsa, kilide gerek yok. SMP
geldiğinde (milestone belirlenmedi) `with()` bir spinlock alacak ve
`unsafe impl Sync` gerçek bir gerekçeye kavuşacak.

Şimdi spinlock eklemek: tek CPU'da hiçbir şey korumayan, ama her
tahsiste bir atomik işlem maliyeti olan bir süs. Eklemiyoruz; yerine
yorumu düzeltiyoruz, çünkü M2'nin yorumu M6'dan sonra **yanlış** olacak.

### Capability modelinde allocator'ın yeri

Asıl soru: `frame_allocator` capability modelinde ne?

**Cevap: hiçbir şey. O, Untyped'ların *altındaki* katman.**

Boot sırasında kernel, Limine'in memory map'inden bir dizi **kök
Untyped** üretir ve hepsini init bileşenine verir. O andan sonra:

```
boot:   frame_allocator  ->  kök Untyped'lar  ->  init bileşeni
sonra:  frame_allocator KULLANILMAZ
```

`frame_allocator` bir boot-zamanı aracı. M5'ten sonra kernel'in çalışma
zamanında çerçeve dağıtan bir yolu olmayacak — tahsis yalnızca retype.

Bu, kilit sorusunu da çözüyor: **boot'tan sonra çağrılmayan bir
allocator'ın eşzamanlılık sorunu yok.** Denetim: `frame_allocator`,
`boot_complete()` çağrıldıktan sonra her `allocate()` çağrısında panic
edecek.

İstisna: heap. Aşağıda.

## Bölüm B — Heap'in bütçesi: ayrıcalıklı istisna, açıkça

### Soru

`heap_frame_source()` bir Untyped'a bağlanacak mı, yoksa kernel'in
ayrıcalıklı istisnası mı kalacak?

### Karar: ayrıcalıklı kalıyor, sınırlı ve ölçülen bir havuzla

Heap, boot sırasında ayrılan **sabit bir çerçeve havuzundan** besleniyor.
Bir Untyped'a bağlanmıyor.

```
boot:
    heap_reserve = N çerçeve, frame_allocator'dan
    kalan her şey -> kök Untyped'lar -> init

sonra:
    heap yalnızca heap_reserve'den büyür
    tükenirse: panic, "kernel heap reserve exhausted"
```

### Neden Untyped'a bağlamıyoruz

Cazip olan şu olurdu: kernel'in de bir Untyped'ı olsun, heap ondan
beslensin, böylece kernel de modelin içinde olsun. Üç sorun:

**1. Döngüsel bağımlılık.** Heap'i Untyped'a bağlamak demek, heap
büyürken bir retype yapmak demek. Retype bir cap slot'u gerektirir, slot
bir CNode'da durur, CNode heap'te değil ama retype'ın kendi defteri
heap'te olabilir. Çözülebilir ama kırılgan.

**2. Kimin Untyped'ı?** Kernel'in heap'i kimin hesabına yazılacak? Eğer
init bileşeninin hesabına yazılırsa, init'in belleği kernel'in iç
işlerine göre dalgalanır — ve init, kernel'in ne zaman heap
kullanacağını bilemez. Eğer kernel'in kendi Untyped'ı varsa, o zaten
"ayrıcalıklı istisna", sadece Untyped kılığında.

**3. Heap'in tükenmesi kernel hatası, user hatası değil.** Bir user
bileşeni kernel'i heap tüketerek çökertebiliyorsa, bu zaten bir DoS
açığı — Untyped'a bağlamak onu *gizler*, çözmez. Çözüm, kernel'in heap
kullanımını sınırlı tutmak.

### Dürüst kabul

**Bu bir istisnadır ve modelin saflığını bozar.** Yazmaktan kaçınmıyorum:

> PythOS'ta kernel'in, hiçbir bileşenin hesabına yazılmayan sabit bir
> bellek rezervi vardır. Bu, "kernel kendi adına allocate etmez"
> kuralının istisnasıdır.

İstisnayı savunulabilir kılan üç şey:

- **Sabit.** Rezerv boot'ta belirlenir ve büyümez. Bir user bileşeni
  onu tüketerek sistemi çökertemez, çünkü miktarı kendi davranışına
  bağlı değil.
- **Ölçülen.** Boot çıktısı rezervin boyutunu ve kullanımını basacak.
  M13'e gelindiğinde "kernel ne kadar heap kullanıyor" sorusunun sayısal
  bir cevabı olacak.
- **Azaltılabilir.** Kernel'in heap kullanımı ne kadar azalırsa rezerv o
  kadar küçülür. İdeal son durum: heap hiç kullanılmıyor ve rezerv sıfır.

### Kernel heap'i gerçekten ne için kullanıyor?

Şu an: hiçbir şey. M4'te heap yazıldı ama kernel'in kendisi `Box` ya da
`Vec` kullanmıyor — yalnızca `heaptest` kullanıyor.

Bu, rahatsız edici ve aydınlatıcı bir gerçek: **M4'te yazdığımız heap,
M5'in nesne modelinde belki hiç gerekmeyecek.** Capability modelinde her
nesne bir Untyped'dan geliyor; kernel'in dinamik bir yapıya ihtiyacı
olan yerler (bekleyen thread kuyrukları, derivation listesi) nesnelerin
*içinde* intrusive olarak duruyor.

Dolayısıyla **rezerv başlangıçta çok küçük tutulacak** (16 sayfa = 64
KiB) ve M13'e kadar artması gerekirse bu, tasarımda bir şeyin yanlış
gittiğinin işareti sayılacak.

*Not:* M4'ün heap'ini "gereksizdi" diye yazmıyorum — `alloc`'un varlığı
test kodunu yazılabilir kılıyor ve M13'ün init sistemi onu kullanabilir.
Ama kernel'in *sıcak yollarında* heap'e ihtiyaç duyulmaması bir tasarım
hedefi, ve rezervin küçük kalması o hedefin ölçüsü.

## Elenen alternatifler

**Allocator'a hemen spinlock koymak.** Tek CPU'da hiçbir şey korumuyor,
her tahsiste bir `lock cmpxchg`. SMP gelince eklenecek; o zamana kadar
tripwire yeterli ve daha bilgilendirici (ihlali *adlandırıyor*).

**Kesme handler'larının tahsis yapmasına izin verip kilitle korumak.**
Kesme yolunda "bellek yok" durumu çözümsüz. Kural olarak yasaklamak,
mekanizma olarak desteklemekten iyi.

**`frame_allocator`'ı tamamen silip her şeyi Untyped yapmak.** Boot
sırasında Untyped'ları üretmek için yine bir şeyin memory map'i okuyup
bölmesi gerekiyor. O şey `frame_allocator`. Silinemiyor, yalnızca
emekliye ayrılıyor.

**Heap'i tamamen kaldırmak.** `alloc` olmadan test kodu yazmak zor ve
M13'ün init sistemi muhtemelen isteyecek. Küçük bir rezerv, tamamen
kaldırmaktan daha dürüst: ihtiyaç varsa görünür olsun.

**Heap rezervini dinamik yapmak (tükenince büyüsün).** O zaman "sabit"
garantisi gider ve kernel'in bellek kullanımı user davranışına bağlanır.
Panic etmek, sessizce büyümekten iyi.

## Sonuç

### Allocator

- `frame_allocator` bir **boot-zamanı aracı**. M5'ten sonra kök
  Untyped'lar üretilip init'e verilir ve allocator emekliye ayrılır.
- `boot_complete()` sonrası `allocate()` çağrısı panic — heap rezervi
  hariç.
- **Kesme bağlamında tahsis yasak**, `interrupt_depth` sayacıyla
  denetlenecek. Kilit değil, tripwire.
- Spinlock SMP ile gelecek. M2'nin `unsafe impl Sync` yorumu M6'dan önce
  güncellenmeli: "kesmeler kapalı" gerekçesi orada geçerliliğini
  yitiriyor.

### Heap

- Heap, boot'ta ayrılan **sabit bir rezervden** beslenir. Untyped'a
  bağlanmaz.
- **Bu, modelin açıkça kabul edilmiş bir istisnasıdır.**
- Rezerv başlangıçta 16 sayfa (64 KiB). Büyümesi gerekiyorsa bu, bir
  tasarım sorununun işareti.
- Tükenirse panic: "kernel heap reserve exhausted".
- Boot çıktısı rezervin boyutunu ve kullanımını raporlayacak.
- `heap_frame_source()`'un `M5 REPLACES THIS` yorumu güncellenecek:
  değiştirilmiyor, **sınırlandırılıyor**.