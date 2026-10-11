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
- M5: `retype`'ın boş-slot kontrolü no-op'a çevrildi → **on dokuz
  senaryonun hepsi geçti.** Aşağıya bakın.

Üçüncüsü kuralın değerini gösteriyor: bozma denemesi, testin *yetersiz*
olduğunu da ortaya çıkarabilir.

### K5'in M5'te kaçırdığı şey — ve kuralın güçlendirilmesi

M5'in ilk teslimi K5'e uyuyordu: bir test bozularak doğrulanmıştı.
Doğrulanmamış olan, `retype`'ın faz 1'indeki `SlotNotEmpty` kontrolüydü.
`return Err(...)` silindiğinde hiçbir şey kırılmadı — çünkü mevcut
atomiklik testi yalnızca `NotEnoughMemory` yolunu zorluyordu ve dolu bir
slot'a hiç retype denenmiyordu.

Bu, K5'in lafzına değil ruhuna aykırı. "Milestone başına en az bir test"
sayısal bir eşik; asıl istenen, **her koruyucunun** kendi yokluğundan
ayırt edilebilmesi. K5 şöyle genişletiliyor:

> Bir fonksiyon `Err` döndüren N farklı yola sahipse, kabul testi o N
> yolun her birini en az bir kez tetiklemelidir. Tetiklenmeyen bir hata
> yolu, var olmayan bir hata yolundan ayırt edilemez.

Pratik karşılığı ucuz: `CapError`'ın her varyantını `captest` içinde
aratmak, hangi hatanın hiç üretilmediğini gösterir. M5'te bu
`SlotNotEmpty`'yi ortaya çıkarırdı.

İkinci bir ders de var, bu sefer testin *şekline* dair. Dolu slot
kontrolü eklenirken ilk akla gelen "ilk slot'u doldur, retype iste"
olurdu. Bu, slot'u yazmadan hemen önce denetleyen bir gerçeklemeyi de
geçirirdi — ve öyle bir gerçekleme, engelden *önceki* slot'ları yazıp
bozardı. Bu yüzden `check_partial_overlap` engeli aralığın **ortasına**
koyuyor:

> Bir toplu işlemin atomikliğini sınarken, engeli ilk öğeye değil
> ortaya koyun. İlk öğe, artımlı bir gerçeklemenin de geçeceği tek
> konumdur.

### K6 — Test fixture'ları kaynak indeksi tahmin etmez, ihtiyacını söyler

M6'da yazılan ve M9a'da ısıran bir desen:

```rust
// kök CSpace'teki 5. Untyped -- neden 5? cunku denendi ve calisti
let untyped = boot::root_untyped(5);
```

M9a'nın IPC testi üçüncü bir işçi thread'i ekleyince bu satır
tükenmiş bir Untyped seçti. İlk düzeltme denemem indeksi
değiştirmekti — aynı şekilde başarısız oldu, çünkü hata indeksin
*değerinde* değil, **indekslemenin kendisinde**.

Bir indeks, test ile boot sırasının ürettiği düzen arasında yazılı
olmayan bir anlaşma kuruyor. Boot sırası değişince anlaşma sessizce
bozuluyor, ve belirti alakasız bir yerde çıkıyor ("retype
NotEnoughMemory").

Kural: **test, hangi kaynağı istediğini değil, kaynaktan ne
beklediğini söylemeli.**

```rust
// ne kadar yer gerektigini soyluyor; hangisi oldugunu aramak
// fixture'in isi
let untyped = boot::root_untyped_with_room(16 * 4096);
```

Bulunamazsa panik mesajı üç şeyi birden taşımalı: **ne istendiği,
kaç aday bakıldığı, en büyük adayda ne kaldığı.** Üçü birlikte "yer
yok" ile "yanlış yere bakıldı" durumlarını ayırt ettiriyor — tek
başına "NotEnoughMemory" ayırt ettirmiyor.

M6'nın `threadtest`'i de bu kurala çevrildi. O zaten çalışıyordu,
ama **şans eseri**: seçtiği 0–4 indeksleri tesadüfen büyük
Untyped'lara denk geliyordu.

### K7 — Makefile `apply_diff` ile düzenlenmez

Bu projede dokuz kez aynı şey oldu: [`Makefile`](../../Makefile)
üzerinde `apply_diff` kullanıldı, araç tab karakterlerini boşluğa
çevirdi ve satır sonlarına CRLF ekledi. Make'in cevabı her seferinde
aynı: `missing separator`.

Hata kodla ilgisiz ve teşhisi ucuz ama tekrarı pahalı. Kural:

- Makefile değişiklikleri [`write_to_file`](../../Makefile) ile,
  dosyanın tamamı yazılarak yapılır.
- Yazıldıktan sonra `make -n <hedef>` ile doğrulanır — hedefin
  çözüldüğünü görmek, dosyanın hâlâ ayrıştırılabildiğinin kanıtı.

Aynı tehlike tab'a anlam yükleyen her dosya için geçerli.

### K8 — Bir test, ölçtüğü mekanizmanın zamanlamasına bağlı olmamalı

M9a'nın tam suite koşusunda M6'nın `threadtest`'i düştü:

```
[PANIC] the spinning thread never ran; there is nothing to preempt
```

Ardından **on dört koşu üst üste geçti**, iterasyon sayıları 31770 ile
316079 arasında. Yani hata gerçekti ama kararsızdı — ve kararsız bir
test, başkasının makinesinde başarısız olacak bir testtir.

Mekanizma: `switch_to` kesmeler kapalıyken çalışıyor, ve yeni
thread'in ilk komutu trampoline'in `sti`'si. O an bekleyen bir timer
kesmesi varsa — tüm switch boyunca maskelenmiş olan — `sti` ile `jmp`
arasında ateşleniyor ve thread kendi ilk komutunu çalıştıramadan geri
alınıyor. Ana thread `SPIN_ITERATIONS`'ı sıfır okuyup "hiç koşmadı"
diyor.

**Oysa koştu.** İlk `fetch_add`'inden önce preempt edildi — testin
göstermek için var olduğu şeyin ta kendisi. Yani tek atışlık sürüm,
mekanizmanın çalışmasını bir arıza olarak raporlayabiliyordu.

Kural:

> Bir testin iddiası, ölçtüğü olayın **gerçekleşip gerçekleşmediği**
> üzerine kurulmalı; o olayın hangi anda gerçekleştiği üzerine değil.

Düzeltme bir döngü, ama gevşetme değil: spinner hiç yield etmediği
için her dönüş hâlâ "CPU ondan alındı" kanıtı. Kalkan tek varsayım,
*ilk* dönüşün spinner ilerledikten sonra olduğuydu. Döngü bir TSC
süresiyle sınırlı, yani gerçekten hiç koşmayan bir thread asılmak
yerine teşhisle düşüyor.

İki yan ders:

- **Teşhis önce, düzeltme sonra.** İlk müdahale yalnızca panik
  mesajını zenginleştirmekti (kaç switch oldu, TCB'nin kendi sayacı
  ne, kuyrukta kaç thread var). Mesaj değişince test geçti — bu da
  arızanın kararsız olduğunu, dolayısıyla bir yarış olduğunu
  söyledi. Mesaj kalıcı: bir dahaki sefere sebep ilk koşuda görünür.
- **Tek yeşil koşu kanıt değil.** Kararsız bir arızadan sonra
  "düzelttim" demek için tek bir geçiş yetmez; burada on dört koşu
  bakıldı ve düşük iterasyon sayıları (31770) yarışın hâlâ gerçek
  olduğunu, sadece artık yakalandığını gösterdi.

### K8'in M14'te ikinci kez ortaya çıkışı — ve `scripts/soak.sh`

M14'ün kabul koşusunda dört `make test`'in ikisi düştü, **farklı
senaryolarda**: biri M11'in `vsuserro`'su, biri M13'ün
`driverdies`'ı. İkisi de "beklenen satır eksik" dedi, ve ikisinin de
log'unda o satır **vardı**.

Sebep log'un kendisinde görünüyordu:

```text
...when they stopped existing[ipc] notification at 0x...4418000 was
 so did it.
```

İki tam mesaj, iç içe geçmiş, ve **hiçbiri artık bir satır değil.**
`print!` bir thread switch'e karşı korunmuyordu: uzun bir mesaj
binlerce `outb` demek, ve M6'dan beri araya bir timer kesmesi girip
başka bir thread'e geçebiliyor. Kurban, tick geldiğinde kim
yazıyorsa o.

Bu M6'dan kalma gizli bir kusurdu, M14'ün değil. Ortaya çıkması için
uzun bir satır ve meşgul bir an gerekiyordu; M14 ikisini de getirdi
(üçüncü bir bileşen ve birkaç çok satırlı rapor).

Düzeltme `print!`'e bir `PreemptGuard` — `InterruptGuard` değil,
ADR 025'in ayrımıyla: satırı bozan şey *thread değişimi*, kesme
değil.

**Ama asıl ders teşhiste.** İlk tanı denemesi yanlış sonuca vardı:
başarısız koşunun seri log'u okundu, temiz çıktı, "tekrarlanamıyor"
denildi. Log temizdi çünkü **sonraki, başarılı koşu onu üzerine
yazmıştı** — her senaryo sabit bir dosya adına yazıyor.

Bu K9'un kardeşi: orada kalıntı bir *artefakt* eksik bir bağımlılığı
gizledi, burada taze bir artefakt bir arızanın kanıtını sildi.

Kural değişmiyor, araç ekleniyor:

> Kararsız bir arıza araştırılırken, koşu döngüsü **ilk kırmızıda
> durmalı ve kanıtı saklamalı.** Bir sonraki koşunun üzerine yazdığı
> bir log, hiç üretilmemiş bir logdan daha kötüdür: var olduğu için
> okunur, ve yanlış şeyi söyler.

[`scripts/soak.sh`](../../scripts/soak.sh) bunu yapıyor: N kez koşar,
ilk başarısızlıkta durur, bütün seri log'ları `build/soak/`'a
kopyalar.

**Kapanmadı, ve öyle yazılıyor.** Düzeltmeden sonra 15 ardışık temiz
`make test` (67/67) ve 15 ardışık `run-ipccapcost` görüldü; öncesinde
4 koşuda 2 arıza vardı. Ama düzeltmeden *sonra* da bir kez
`ipccapcost` düştü — farklı bir belirtiyle (panic), ve o log
yakalanamadan üzerine yazıldı. Oran düştü, sıfırlandığı
gösterilemedi. Bir sonraki milestone bir arıza görürse ilk bakacağı
yer burasıdır.

### K9 — Kabul koşusu `distclean`'den başlar

M12'den sonra, yeniden adlandırma teslimi sırasında yakalandı.
`make test` geçiyordu; `make distclean && make test` geçmiyordu:

```
error: couldn't read `kernel/src/../../build/roottask.bin`
make: *** [Makefile:449: clippy] Error 101
```

Sebep, `clippy`'nin `$(ROOTTASK_BIN)`'e bağlı olmamasıydı. M12
`$(KERNEL)`'e bu bağımlılığı vermişti — kernel, root task imajını
`include_bytes!` ile gömüyor ([ADR 044](044-root-task-ve-bootstrap-protokolu.md)) — ama clippy de kernel'i
*derliyor*, ve `test: fmt clippy run ...` sırasında clippy her şeyden
önce koşuyor.

İlginç olan, hatanın neden M12'de görünmediği. Çünkü `build/` önceki
koşulardan kalmıştı: dosya zaten oradaydı, hiçbir kuralın onu
üretmesi gerekmedi, ve eksik kenar hiçbir belirti vermedi. **Kalıntı
artefakt, eksik bağımlılığı birebir taklit eder** — tıpkı var olan
bir dosya gibi davranır, ta ki temiz bir ağaçta aranana kadar.

Bu K5'in yapısının build sistemine taşınmış hali: eksik bir kenar,
kendi yokluğundan ayırt edilemez. Ve K7'nin kardeşi — K7 Makefile'ın
nasıl *yazıldığıyla*, K9 nasıl *koşturulduğuyla* ilgili.

Kural:

> Bir milestone'un kabul koşusu `distclean`'den başlamalı, ve rapor
> bunu komutun kendisiyle göstermeli. "`make test` geçti" bir
> bağımlılık iddiası taşımaz; yalnızca o anki `build/` içeriğiyle
> geçtiğini söyler.

Yan ders, bu olayda iki kez ortaya çıktı: düzeltme `clippy:` kuralına
yazılamaz. O kural `ROOTTASK_BIN` tanımından ~320 satır önce
ayrıştırılıyor ve make önkoşul listesini **ayrıştırma anında**
genişletiyor; oraya yazılsa boş dizgeye açılır, hata vermez, uyarmaz.
M12 aynı tuzağı `$(KERNEL)` için belgelemişti. İkinci kez, kimsenin
bakmayı düşünmediği bir hedefte.

### K10 — Bir ADR var olmayan bir mekanizmayı tarif edemez

M14'te yazıldı, [ADR 050](050-boot-device-untyped-tohumlama.md)'nin
ilk hâli yüzünden. O ADR şunu söylüyordu:

> Bir device Untyped'ın muhasebesi capability'de yaşar, adlandırdığı
> bellekte değil — ki `size_bits`'in slot'ta olmasının sebebi bu.

Cümle bir **gereksinimi**, sanki bir **olguymuş** gibi yazıyordu.
Öyle bir muhasebe yoktu: `size_bits` bölgenin boyutu, hiç
değişmiyor, ve watermark'ın gidecek bir yeri yoktu.

Kimse fark etmedi çünkü fark edecek bir şey yoktu — cümle doğru
*görünüyordu*, ve ilk gerçek device Untyped ortaya çıkana kadar
hiçbir kod yolu onu sınamadı. Sınadığında kernel
`cr2=0xffff8000fd000000` ile durdu.

Bu, K5'in yapısının **belgeye** taşınmış hâli. K5 diyor ki: bir
koruyucu, kendi yokluğundan ayırt edilebilmeli. K10 diyor ki:

> Bir karar bir mekanizmaya dayanıyorsa, ADR ya o mekanizmayı
> uygulayan kodu **adıyla** göstermeli ya da "henüz yok" diye
> işaretlemeli. Tarif edilmiş ama var olmayan bir mekanizma, var
> olan bir mekanizmadan ayırt edilemez — ta ki ilk müşterisi gelene
> kadar.

İki kabul edilebilir biçim:

```markdown
<!-- (a) adıyla -->
Watermark `untyped.rs`'deki `DEVICE_UNTYPEDS` tablosunda,
base adresle anahtarlı (`device_watermark`).

<!-- (b) borç olarak -->
Watermark'ın nerede duracağı **henüz çözülmedi**. Bugün device
Untyped yok, dolayısıyla soru da yok; ilk gerçek aygıtı ekleyen
milestone bunu cevaplamak zorunda.
```

(b) daha az tatmin edici ve daha dürüst. ADR 017'nin "boot device
Untyped üretir" cümlesi de aynı hatanın daha büyük hâliydi: dokuz
milestone boyunca bir niyet, bir olgu kılığında durdu.

Bu ADR'nin kendi geçmişinde de bir örneği var: M9c, `cnode.rs`'de
"bir sonraki milestone bu arm'ı eklemek zorunda kalacak" diye not
bıraktı ve bırakmakla kalmadı — `match`'i exhaustive yaparak
derleyiciyi o notun uygulayıcısı yaptı. **(b)'yi derleyiciye
söyletebiliyorsanız, söyletin.**

### K11 — Bir kural, yazıldığı bağlamı taşır

K10'un kardeşi, ve aynı milestone'da doğdu. K10 *yazılanın var
olmamasıyla* ilgili; K11 *yazılanın her yerde geçerli
sanılmasıyla*.

[ADR 020](020-retype-semantigi.md) üç kural koydu, üçü de M5'te,
üçü de "bir Untyped'ın belleği yazılabilir RAM'dir" varsayımı
altında:

| Kural | M5'teki ifadesi | M14'te |
|---|---|---|
| Untyped kendi başlığını taşır | ilk 16 bayt | device'ta başlık yok |
| watermark başlıkta | ilk 8 bayt | kernel tablosunda |
| retype **her zaman** sıfırlar | "istisna yok" | device muaf |

Üçü de dokuz milestone boyunca doğru kaldı, ve ilk gerçek aygıt
belirdiğinde **aynı anda** kırıldı. Hiçbiri yanlış yazılmamıştı;
hepsi, yazıldıkları bağlamın dışına taşındıklarında yanlış oldu.

> Bir kuralı yeni bir bağlama genişletmeden önce, **hangi
> varsayımlar altında doğru olduğu** sorulmalı. Varsayım yazılı
> değilse, kuralı ilk kez farklı bir şeye uygulayan kişi onu
> bulmak zorunda — ve genellikle bir `#PF` ile bulur.

Pratik karşılığı, K5'inki kadar ucuz: bir kural yeni bir tür
kaynağa uygulanırken, o kuralın gerekçesindeki her cümle için
"bu, yeni kaynak için de doğru mu?" diye sorulur. ADR 020'nin
sızıntı gerekçesi bu soruyu geçemedi — çünkü "önceki sahibin
kalıntısı" kavramı, durumu cihazın kendisine ait olan bir bellek
için anlamsız.

Bu soruyu sormanın maliyeti bir paragraf; sormamanın maliyeti M14'te
üç ayrı hata ayıklama turu oldu.

### K12 — Bir doğrulama, doğruladığı şeyin tamamını okumalı

M15'te doğdu, ve K5'in donanım tarafındaki hâli.

K5 diyor ki: başarısız olamayan bir test, kendi yokluğundan ayırt
edilemez. K12 daha sinsi bir hâlini tarif ediyor — **başarısız
olabilen ama yalnızca kendi yazdığına bakan** bir doğrulama.

`ioapic::verify` M12'de yazıldı ve üç milestone boyunca her koşuda
"bütün hatlar uyumlu" dedi. Yalan söylemiyordu: baktığı tek alan
maske bitiydi, maske biti yazılıyordu, ve geri okunuyordu.

Bir I/O APIC yönlendirme girdisi 64 bit. Kernel bunun **bir** bitini
yazıyordu; diğer 63'ü — vektör, teslim kipi, hedef, polarite,
tetikleme — firmware'in bıraktığı değerdeydi. Maskesi kaldırılan
böyle bir hat hiçbir yere teslim etmiyor.

M15'te ilk gerçek cihaz hattı (IRQ 1) geldiğinde kesme hiç
ulaşmadı, ve `verify` hâlâ "hepsi uyumlu" diyordu.

> Bir doğrulama, **yazdığı alanı değil, doğruladığı nesnenin
> tamamını** okumalı. Yalnızca yazdığına bakan bir `verify`,
> yazılmamış alanların yokluğunu kanıtlayamaz — çünkü o alanların
> var olması gerektiğini bilmiyor.

Pratik karşılığı: bir donanım yapısını doğrulayan kod, o yapının
alan listesini kaynaktan (datasheet, SDM) alıp hepsini
karşılaştırmalı. Atlanan her alan, "bu alan önemsiz" iddiasıdır ve
yazılmalıdır.

Düzeltilmiş hâli iki şey yapıyor: `verify` girdinin tamamını okuyor
ve kernel'in **niyetiyle** karşılaştırıyor — niyet ayrı bir tabloda,
çipin gölgesi olarak değil, çünkü gölge zaten kernel'in yazmak
istediğiyle hemfikir olurdu. Ayrıca `kbdtest` çipten doğrudan
okuyup "IRQ 1 vektör 34'te" diyor, yani iddia kabul testinde de
görünür.

Bedeli: `verify`'da otuz satır ve bir `u8` dizisi. Sormamanın
bedeli beş milestone boyunca sessiz kalan bir eksiklikti.

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
  doğrulanmalı ve bu rapora yazılmalı. Genişletilmiş hali: her `Err`
  yolu en az bir kez tetiklenmeli.
- **K6:** Fixture'lar kaynak indeksi tahmin etmez; ihtiyacı
  bildirilir, arama fixture'ın işidir. Bulunamazsa panik mesajı
  istenen/bakılan/en iyi adayı birlikte yazar.
- **K7:** Makefile `apply_diff` ile düzenlenmez; tam yazılır ve
  `make -n` ile doğrulanır.
- **K8:** Bir test, ölçtüğü olayın gerçekleşmesine dayanmalı, o olayın
  zamanlamasına değil. Kararsız bir arıza tek yeşil koşuyla kapanmaz.
- **K9:** Kabul koşusu `distclean`'den başlar; kalıntı bir artefakt
  eksik bir bağımlılığı gizler. Rapor komutu olduğu gibi göstermeli.
- **K10:** Bir ADR var olmayan bir mekanizmayı tarif edemez. Ya kodu
  adıyla gösterir ya "henüz yok" der; mümkünse derleyiciye söyletir.
- **K11:** Bir kural yazıldığı bağlamı taşır. Yeni bir kaynak türüne
  genişletmeden önce gerekçesinin her cümlesi yeniden sorgulanır.
- **K12:** Bir doğrulama, doğruladığı şeyin tamamını okumalı.
  Yalnızca kendi yazdığı alana bakan bir `verify`, yazılmamış
  alanların yokluğunu kanıtlayamaz.
- Otomatik denetim ertelendi; üç yol değerlendirildi ve her birinin
  maliyeti yazıldı.
- M6+ testleri bu kurallara uyacak. Mevcut testler M4'te zaten
  düzeltildi.