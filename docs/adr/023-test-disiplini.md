# ADR 023 — Testler optimize edilip silinemez

Status: accepted (M5 architect pass)

M4'ten devreden madde. İki kez aynı hata yapıldı; üçüncüsü olmaması için
kural yazılıyor.

## Bağlam

Bu projede testler kernel'in içinde çalışıyor: `memtest`, `vmtest`,
`heaptest` gerçek donanımda gerçek invariant'ları kontrol ediyor, ve
sonuçlarını seri porta basıyor. Host tarafındaki
[`run.py`](../../scripts/run.py) o çıktıyı denetliyor.

Bu yaklaşımın bir zayıflığı var ve iki kez ısırdı.

### Olay 1 — M3, yığın taşması

`vmtest::recurse` yığını tüketip guard sayfasına çarpmalıydı. İlk
sürümü:

```rust
fn recurse(depth: u64) -> u64 {
    let burn = [0u8; 1024];       // kullanılmıyor
    recurse(depth + 1)
}
```

LLVM `burn`'ü sildi — kimse okumuyor — ve fonksiyon yığın büyütmeyen bir
döngüye dönüştü. Test hiçbir şey taşırmadı.

### Olay 2 — M4, heap tükenmesi

`heaptest::run_oom` heap'i doldurmalıydı:

```rust
let ptr = unsafe { alloc(layout) };
assert!(!ptr.is_null());          // null kontrolü "kullanım" değil
```

LLVM `alloc`'u bir ayırma fonksiyonu olarak tanıyor ve sonucu
kullanılmayan bir ayırmayı silmeye yetkili. 72 ayırma hiç olmadı; test
"heap sınırını aştı" diye bağırdı, oysa heap'e hiç dokunulmamıştı.

### Ortak yapı

Her iki olayda da:

- Test **yeşil değil, yanlış kırmızı** verdi — şanslıydık. Yanlış yeşil
  verseydi fark etmezdik.
- Hata test kodundaydı, kernel'de değil.
- Belirti, test ettiği şeyle ilgisizdi ("72 MiB dağıtıldı" mesajı,
  aslında "0 MiB dağıtıldı" demekti).

Üçüncüsü en tehlikeli özellik: **testin yalanı, test ettiği bileşenin
hatası gibi görünüyor.**

## Kural

### K1 — Kaynak tüketen bir testin tüketimi gözlemlenebilir olmalı

Bir test "şu kaynak tükensin" diyorsa, tükettiğini kanıtlayan bir
gözlem yapmalı. Üç kabul edilebilir yol:

```rust
// (a) volatile yazma — tahsis edilen belleğe dokun
unsafe {
    ptr.write_volatile(PATTERN);
    ptr.add(size - 1).write_volatile(PATTERN);
}

// (b) volatile okuma — yazılanı geri oku
let seen = unsafe { ptr.read_volatile() };
assert_eq!(seen, PATTERN);

// (c) durumu sorgula ve iddia et — en güçlüsü
let stats = heap::stats();
assert!(stats.mapped_pages > before, "the heap did not actually grow");
```

(c) tercih edilen: yalnızca optimizasyonu engellemiyor, **doğru şeyin
olduğunu** da söylüyor. (a) ve (b) yalnızca "bir şey oldu" diyor.

`core::hint::black_box` de kullanılabilir ama tercih edilmiyor: optimize
ediciye bir ipucu, bir garanti değil, ve okuyana *neden* orada olduğunu
söylemiyor.

### K2 — Her testin bir sayısal çıktısı olmalı

"Geçti" yetmez. Test, ne yaptığını sayıyla söylemeli:

```
[heaptest] alloc: 10000 blocks, 1 B .. 3 KiB, every one aligned
[heaptest] growth: heap grew from 2868 to 3071 pages
[memtest] alloc: 10000 unique, aligned, in-range (frames 84..=10180)
```

Sebep: bir sayı yanlış olduğunda göze çarpar. "10000 blok" yazan bir
satır, gerçekte 0 blok ayrılmışsa ya hiç basılmaz ya 0 yazar — iki
durumda da `run.py`'nin regex'i tutmaz.

M4'ün OOM testi bu kurala uysaydı ilk çalıştırmada yakalanırdı: "N MiB
allocated, M pages mapped" satırındaki M sıfır kalırdı.

### K3 — `run.py` sayıları da denetlemeli, yalnızca varlığı değil

Regex `\d+` yerine, anlamlı bir alt sınır koymalı:

```python
# zayıf: sıfır da geçer
re.compile(r"^\[heaptest\] \d+ MiB allocated and leaked, \d+ pages mapped")

# güçlü: dört haneli sayfa sayısı, yani gerçekten büyümüş
re.compile(r"^\[heaptest\] \d+ MiB allocated and leaked, \d{4,} pages mapped")
```

M4'te bu düzeltme yapıldı. Kural olarak yazılıyor: **bir testin
raporladığı sayı, testin başarısızlığında farklı olacak şekilde
denetlenmeli.**

### K4 — Negatif testler ne yapmadıklarını da söylemeli

Bir testin yakalamaya çalıştığı hata gerçekleşmezse, bunu açıkça
söylemeli. `vmtest::run_stale_tlb` örneği:

```
[vmtest] read after unmap returned 0x... -- QEMU did not fault;
         this check is advisory
```

Sessizce geçmek yerine "beklediğim olmadı" demek, M3'te `invlpg`'nin
QEMU'da ölçülemediğini keşfetmeyi sağladı.

## Nasıl denetlenecek

**Bugün: inceleme.** M6+ testleri yazılırken K1–K4 gözden geçirilecek.

**İdeal: otomatik.** Düşünülen ve elenen yollar:

- *Üretilen assembly'yi denetlemek.* Test fonksiyonunun `call
  __rust_alloc` içerdiğini doğrulamak mümkün ama kırılgan: inline
  edilirse sembol kaybolur.
- *Testi iki kez, farklı optimizasyon seviyelerinde çalıştırmak.* `-O0`
  ile `-O3` farklı sonuç veriyorsa bir şey silinmiş demektir. Maliyeti
  iki kat derleme ve iki kat QEMU; ve `-O0` kernel'i farklı davranabilir
  (yığın kullanımı artar). Ertelendi.
- *Mutasyon testi.* Test ettiği kodu bozup testin kırıldığını görmek. Bu
  projede elle yapılıyor zaten — M3'te SAFETY yorumu silindi, M3'te
  linker script bozuldu, M3'te `invlpg` silindi. **Kural olarak
  yazılıyor:** her milestone'da en az bir test, test ettiği şeyi
  bozarak doğrulanmalı.

K5 olarak ekliyorum:

### K5 — Her milestone'da en az bir test, bozarak doğrulanmalı

Bir testin gerçekten bir şey denetlediğinin tek kanıtı, denetlediği şey
bozulduğunda kırılmasıdır. Milestone raporu, hangi testin nasıl
bozularak doğrulandığını yazmalı.

Geçmiş örnekler:
- M3: `// SAFETY:` yorumu silindi → clippy durdu, QEMU hiç başlamadı.
- M3: `__writable_start` yanlış yere taşındı → `check_kernel.py` yakaladı.
- M3: `invlpg` + `mfence` silindi → **hiçbir test kırılmadı**, ki bu da
  bir bulgu ([hardware-todo.md](../hardware-todo.md)).

Üçüncüsü kuralın değerini gösteriyor: bozma denemesi, testin *yetersiz*
olduğunu da ortaya çıkarabilir.

## Elenen alternatifler

**`#[inline(never)]` + `volatile` her yerde, kural olarak.** Fazla geniş:
her testi okunamaz hale getirir ve asıl soruyu (bu test ne kanıtlıyor?)
gizler.

**Testleri ayrı bir crate'te, `opt-level = 0` ile derlemek.** Kernel
testleri kernel'in içinde çalışıyor ve kernel'le aynı ayarlarla
derlenmeli — yoksa test edilen şey gönderilen şey değil
([ADR 002](002-tek-binary-iki-iso.md)'nin "aynı binary" ilkesi).

**`black_box`'a güvenmek.** Bir ipucu, garanti değil. Dokümantasyonu
açıkça "optimize edici bunu yok sayabilir" diyor.

**Hiçbir şey yapmamak, dikkatli olmak.** İki kez denendi, iki kez
başarısız oldu.

## Sonuç

- **K1:** Kaynak tüketen testler tüketimi gözlemlemeli — tercihen durum
  sorgulayarak, yoksa volatile ile.
- **K2:** Her test sayısal çıktı vermeli.
- **K3:** `run.py` o sayıları anlamlı sınırlarla denetlemeli; `\d+`
  yetmez.
- **K4:** Negatif testler beklenen olay gerçekleşmediğinde bunu
  söylemeli.
- **K5:** Her milestone'da en az bir test, test ettiği şeyi bozarak
  doğrulanmalı ve bu rapora yazılmalı.
- Otomatik denetim ertelendi; üç yol değerlendirildi ve her birinin
  maliyeti yazıldı.
- M6+ testleri bu kurallara uyacak. Mevcut testler M4'te zaten
  düzeltildi.