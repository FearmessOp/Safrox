# Gerçek donanımda doğrulanacaklar

Bu dosya, **QEMU altında test edilemediği için doğrulanmamış** kalan
iddiaları topluyor. Her madde şunları söylüyor: iddia ne, neden burada
sınanamıyor, gerçek makinede nasıl sınanacak, ve yanlış çıkarsa ne olur.

Bir şeyin bu listede olması "muhtemelen bozuk" demek değil. "Testlerin
geçmesi bu konuda hiçbir şey kanıtlamıyor" demek — ki test takımına
güvenirken bilinmesi gereken bir şey.

Güncelleme: M15.

---

## 1. `invlpg` çalışma zamanında hiçbir şey yapmıyor (M3)

**İddia** ([ADR 012](adr/012-tlb-gecersizlestirme.md)): bir eşleme
değiştikten sonra TLB geçersizleştirilmezse CPU eski çeviriyi kullanmaya
devam eder, ve bu projenin en pahalı hata sınıfı.

**Ölçüm.** `TlbFlush::flush`'ın gövdesi tamamen boşaltıldı — ne `invlpg`
ne `mfence` — ve tam kabul takımı çalıştırıldı:

```
12 senaryodan 12'si geçti.
```

Yani bu ortamda `flush()`'ın çalışma zamanı değeri **sıfır**. `TlbFlush`
tipinin sağladığı her şey derleme zamanında: `#[must_use]` ve panic eden
`Drop`, invalidation'ı *unutmayı* imkânsız kılıyor — unutmanın burada
gözlemlenebilir olup olmamasından bağımsız olarak.

**Neden QEMU'da sınanamıyor.** QEMU'nun softmmu'su gerçek bir TLB değil;
bir çeviri önbelleğini modelliyor ve sayfa tablosu yazmalarını büyük
ölçüde anında görüyor. Mimarinin izin verdiği "eski girdiyle devam etme"
davranışını üretmiyor.

**Gerçek donanımda.** `vm-stale-tlb` senaryosunu çalıştır. Eşle, yaz,
geçersizleştirmeden çöz, oku. Gerçek silikonda okumanın **eski veriyi
döndürmesi** bekleniyor; QEMU'da da öyle oluyor (madde 2), ama orada bu
TLB'den değil.

Daha güçlü bir test: `flush()`'ı yine boşalt ve kabul takımını gerçek
makinede çalıştır. **En az bir senaryonun kırılması** gerekir. Kırılmazsa
ya testler yetersiz ya da iddia yanlış — ikisi de öğrenmeye değer.

**Yanlış çıkarsa.** `TlbFlush`'ın tip düzeyindeki maliyeti (her çağrı
sitesinde bir `.flush()`) gereksiz olurdu. Ama kaldırmak için kanıt
gerekir, tersi için değil.

---

## 2. Stale-TLB senaryosunun sonucu yanıltıcı olabilir (M3)

**Gözlem.** `vm-stale-tlb` çalıştığında QEMU **fault vermiyor** ve okuma
eski veriyi döndürüyor:

```
[vmtest] read after unmap returned 0x50474e4950414d56 -- QEMU did not fault;
         this check is advisory
```

M3 raporunda bunu "tehlikenin canlı gösterimi" diye yazdım. **Bu çıkarım
madde 1 ile birlikte okununca şüpheli hale geliyor:** `invlpg` zaten hiçbir
şey yapmıyorsa, okumanın başarılı olması bir TLB girdisinin hayatta
kaldığını değil, QEMU'nun çeviriyi başka bir yoldan çözdüğünü gösteriyor
olabilir. Örneğin unmap'ten sonra bile sayfa tablosu girdisini önbelleğe
almış olabilir ya da `ignore_because_caller_will_flush_all` yüzünden
girdinin hiç geçersizleştirilmemiş olması, QEMU'nun kendi çeviri
önbelleğinde kalmasına yetiyor olabilir.

**Gerçek donanımda.** Aynı senaryo. Beklenen iki sonuçtan hangisinin
gerçekleştiğini **ve nedenini** kaydet:

- Eski veri dönüyorsa: bayat TLB girdisi gerçek, ADR 012 haklı.
- #PF geliyorsa: donanım girdiyi kendiliğinden düşürmüş; bu da mimarinin
  izin verdiği bir davranış ve ADR 012'yi yanlışlamaz, sadece testin
  deterministik olmadığını gösterir.

Her iki durumda da senaryo advisory kalmalı — asıl kanıt madde 1'deki
deney.

---

## 3. `mfence` gerçekten gerekli mi (M3)

**İddia.** `TlbFlush::flush` içinde sayfa tablosu yazması ile `invlpg`
arasında bir `mfence` var. Gerekçe: Intel SDM 4.10.4.1 bu sıralamayı açıkça
garanti etmiyor ve ölçülemeyen bir yerde en kötü durum varsayılıyor.

**Neden QEMU'da sınanamıyor.** QEMU komutları program sırasında
yürütüyor; `mfence`'in engellediği türden bir yeniden sıralama hiç
olmuyor. Kaldırıldığında da (madde 1'deki deney, `mfence`'i de kaldırdı)
hiçbir test kırılmıyor.

**Gerçek donanımda.** Mikro-mimari yeniden sıralamayı gözlemlemek tek bir
testle zor. Pratik yaklaşım: SDM'nin ilgili bölümünü bir donanım
mühendisiyle ya da Intel forumlarında netleştirmek, ve `mfence`'i
gerçekten gerekli kılan bir senaryo olup olmadığını belgelemek. Gerekli
değilse kaldırmak bir komut kazandırır — ama "ölçemedim, o yüzden
koydum" savunulabilir bir pozisyon olmaya devam ediyor.

---

## 4. SMEP ve SMAP (M3 → M4 → **M8'de QEMU tarafı kapandı**)

**Durum: kısmen çözüldü.** M3'te `-cpu qemu64` bu bitleri bildirmiyordu,
yani kod yolu çalışıyor ama koruma yoktu. M4'te `QEMU_FLAGS` artık
`-cpu qemu64,+smep,+smap` kullanıyor ve boot çıktısı:

```
[cpu] smep: enabled, smap: enabled, pge: enabled
```

`run.py` bu satırı her senaryoda zorunlu tutuyor, yani CPU modelinin
sessizce geri alınması test hatası.

**M8 geldi ve üçü de gösterildi.** Artık korunacak user sayfaları
var.

`user-smep`: bir user kod sayfası, ring 3'ten **101 kez** çalıştı,
sonra aynı sayfa CPL 0'dan getirildi:

```
[trap] error_code=0x0000000000000011  rip=0x0000000000400000  cs=0x0008
[trap] cr2=0x0000000000400000  (supervisor instruction fetch
                                protection-violation)
```

Bit 4 (instruction fetch) ve bit 0 (present) birlikte: sayfa var,
çalıştırılabilir, ve yine de reddedildi. SMEP'ten başka bir şey bu
kombinasyonu üretmiyor.

**Aynı sayfanın** kullanılması kasıtlı. Farklı bir sayfa olsaydı,
"biri çalışıyor, diğeri faultluyor" gözlemi bir eşleme farkıyla da
açıklanabilirdi; test o zaman SMEP'i değil, kendi kurulumunu ölçerdi.

`user-smap`: kernel aynı user sayfasını iki kez okudu. `UserAccess`
guard'ı ile başarılı; guard'sız:

```
[trap] cr2=0x0000000000600000  (supervisor read protection-violation)
```

İkisi tek senaryoda, çünkü ayrı ayrı her biri iddianın yarısını
kanıtlıyor. Guard'lı okuma **önce** koşuyor — guard'sız olan VM'i
bitiriyor.

**Hâlâ doğrulanmamış olan.** QEMU'nun bu bitleri doğru uyguladığı
varsayımı. Gözlemlenen davranış mimarinin tarif ettiğiyle birebir
örtüşüyor, ama bu QEMU'nun doğru modellediğini gösterir, silikonun
aynısını yapacağını değil.

**Gerçek donanımda.** `user-smep` ve `user-smap` ISO'larını USB'den
boot et, seri çıktıdan error code'ları oku. Beklenen: aynı iki satır.
Farklı çıkarsa ya QEMU yanlış modelliyor ya da bu CPU'nun SMEP/SMAP
davranışı farklı — ikisi de bilinmeye değer.

Not: `-cpu qemu64` bu bitleri bildirmiyor; `+smep,+smap` olmadan
senaryolar **vacuous geçmez**, açıkça panic eder ("SMEP is not
enabled on this CPU, so this scenario cannot demonstrate anything").
M3'te sessizce hiçbir şey test etmeyen durumun tekrarlanmaması için.

---

## 5. Frame allocator kilitsiz (M2'den beri)

**İddia** ([ADR 007](adr/007-bitmap-frame-allocator.md)): tek CPU,
kesmeler kapalı, her giriş noktası yeniden girişsiz tek bir fonksiyondan
geçiyor — bu yüzden kilit gerekmiyor.

**Neden sınanamıyor.** Tek CPU ile çalışıyoruz. İddia doğru *ve* test
edilemez; ikinci CPU geldiği an yanlış olacak.

**Ne zaman.** SMP geldiğinde (milestone henüz belirlenmedi). O noktada
bu bir "doğrulanacak" madde değil, "yeniden yazılacak" madde.

---

## Nasıl çalıştırılır

Gerçek donanımda test, USB'den boot etmeyi ve seri çıktıyı yakalamayı
gerektiriyor. ISO'lar hibrit (BIOS + UEFI), yani `dd` ile USB'ye
yazılabiliyor:

```sh
sudo dd if=build/safrox-vmtest.iso of=/dev/sdX bs=4M status=progress
```

Seri çıktı için ya gerçek bir COM portu ya bir USB-seri adaptör gerekiyor;
`isa-debug-exit` gerçek donanımda yok, yani kernel çıkış kodu yerine son
satıra bakılacak. `run.py` bu senaryo için kullanılamaz — elle
doğrulama.

---

## 6. `sysret`'in kanonik olmayan RCX davranışı (M8)

**İddia** ([ADR 029](adr/029-ring-3-giris-ve-cikis.md)): `sysretq`,
RCX kanonik değilse Intel'de `#GP` veriyor — ve bu `#GP`, SS zaten
user değerini almışken ama stack hâlâ kernel'inken alınıyor. Yani
handler, user'ın etkileyebildiği bir yığında CPL 0'da çalışıyor.
CVE-2012-0217 bu.

Kernel bu yüzden `sysret` öncesi RCX'i kontrol ediyor ve kanonik
değilse `iretq`'e düşüyor.

**Ölçüm.** `user-test` koşusunda:

```
[syscall] 9810054 calls (0 returned an error), 9810054 via sysret,
          0 forced onto iretq by a non-canonical rcx
```

Yani kontrol **hiç tetiklenmedi.** Çalıştığına dair kanıt sadece üç
komutun orada durması; davranışsal bir kanıt yok.

**Neden QEMU'da sınanması zor.** Tetiklemek için user kodunun kanonik
olmayan bir adrese dönmeyi denemesi gerekiyor, ki bunun için RCX'i
`syscall`'dan *sonra* bozması lazım — ama `syscall` RCX'i kendisi
yazıyor, yani user kodu araya giremiyor. Tek yol kernel'in frame'deki
RCX'i kasten bozması, ki o da gerçek senaryoyu değil testi test
ediyor.

Ayrıca AMD ve Intel bu durumu **farklı** ele alıyor, ve QEMU'nun
hangisini modellediği belli değil. Farkın kendisi tehlikenin parçası:
bir platformda çalışan kod diğerinde açık veriyor.

**Yapılacak.** İki iş var ve ikisi de ertelendi:

1. Kernel tarafından RCX'i bozan bir senaryo (`user-sysret-canonical`),
   ki en azından `iretq` yolunun koştuğu gösterilsin. Testi test
   etmek olduğu için değeri sınırlı ama sıfır değil — şu an o yola
   hiç girilmiyor ve bir derleme hatası bile fark edilmez.
2. Gerçek donanımda, Intel ve AMD'de ayrı ayrı. Beklenen: `iretq`
   sayacı sıfırdan büyük, kernel ayakta.

**Yanlış çıkarsa.** Kontrol gereksizse üç komut kazanılır. Gerekliyse
ve yanlış yazılmışsa — bu bir ayrıcalık yükseltme açığı.

---

## 7. Tek CPU varsayımı syscall yoluna girdi (M8)

**İddia** ([ADR 030](adr/030-syscall-abi.md)): `syscall` RSP'ye
dokunmuyor, ve kernel stack'ine geçmek için `swapgs` yerine iki
RIP-göreli `static` kullanılıyor: `KERNEL_RSP` ve `SAVED_USER_RSP`.

**Neden doğru, ve ne zaman yanlış olacak.** Tek CPU'da iki static
yeterli ve `swapgs`'in tuzaklarından kaçınıyor (her exception
handler'ında "nereden geldim" kontrolü). İkinci CPU geldiği an
yanlış: iki CPU aynı `SAVED_USER_RSP`'ye yazarsa biri diğerinin user
RSP'sini kaybeder, ve o thread user'a dönerken başka bir thread'in
yığınına döner.

**Neden sınanamıyor.** Tek CPU ile çalışıyoruz. Madde 5 ile aynı
durum: iddia doğru *ve* test edilemez.

**Ne zaman.** SMP geldiğinde bu bir "doğrulanacak" madde değil,
"yeniden yazılacak" madde — `swapgs` ve CPU-başına veri. ADR 030
bunu bilinen borç olarak kaydetti.

---

## 8. AVX kapalı tutularak kaçınıldı (M8)

**İddia** ([ADR 031](adr/031-fpu-sse-durumu.md)): `fxsave` 512 bayt
ile x87, MMX ve XMM0-15'i kaydediyor; AVX durumu (YMM'nin üst yarısı)
kaydedilmiyor. Bu güvenli, çünkü CR4.OSXSAVE temiz tutuluyor ve user
kodu AVX komutlarını çalıştıramıyor — `#UD` alıyor.

**Doğrulanmamış olan.** "OSXSAVE temizse AVX erişilemez" iddiası
test edilmedi. Bir user thread'inin AVX komutu denediği ve `#UD`
aldığı bir senaryo yok.

**Neden önemli.** Yanlışsa sessiz: bir user thread'i YMM'nin üst
yarısına yazar, preempt edilir, ve geri döndüğünde orada başka bir
thread'in verisi olur. Bir bilgi sızıntısı, ve hiçbir şey hata
vermez.

**Yapılacak.** `user-avx` senaryosu: ring 3'ten `vzeroupper` ya da
`vmovdqu` dene, `#UD` bekle. Hem QEMU'da hem donanımda —
`-cpu qemu64` AVX bildirmiyor, yani QEMU'da `#UD` yanlış sebepten
gelebilir ve test vacuous olur. Donanımda anlamlı.

---

## 9. Pitch kullanıldığı QEMU'da kanıtlanamıyor — mutasyon hayatta kaldı (M14)

**İddia** ([ADR 049](adr/049-bootinfo-framebuffer.md)): bir satırın ilk
pikseli `y * pitch` adresinde, `y * width * bpp / 8` adresinde değil.
Donanım satırları kendi hoşlandığı bir hizaya dolduruyor ve aradaki
baytlar kimseye ait değil.

**Ölçüm — ve bu listedeki en net sonuç.** `Geometry::offset` içindeki
`pitch`, `width * bpp / 8` ile değiştirildi:

```rust
fn offset(&self, x: u64, y: u64) -> u64 {
    y * (self.width * (self.bpp / 8)) + x * (self.bpp / 8)
}
```

Dört M14 senaryosunun dördü de geçti. **Mutasyon hayatta kalıyor.**

Sebep aritmetik: QEMU'nun bildirdiği geometri 1280×800×32, yani
`pitch = 5120` ve `width * bpp / 8 = 1280 * 4 = 5120`. İki ifade
**aynı sayı**. Doldurma yok, dolayısıyla doldurmayı yok saymanın bir
bedeli de yok.

**Bu bir test eksikliği değil, ortamın sınırı.** ADR 023'ün K5'i
"her koruyucu kendi yokluğundan ayırt edilebilmeli" diyor; burada
koruyucu ile yokluğu **bu makinede özdeş**. Yapılabilecek en dürüst
şey, testin bunu her koşuda söylemesi — `fbtest::report_pitch`
yapıyor:

```
[fb] pitch 5120 bytes equals width x bpp / 8 on this machine, so
     criterion 5 cannot be falsified here. MUTATION SURVIVES: ...
```

Alternatif — "ikisi farklı olmalı" diye assert etmek — elimizdeki tek
donanımda senaryoyu kırardı, yani emülatörü test etmek olurdu.

**Ne kontrol ediliyor.** Bileşen row 1'in başına bir imleç yazıp aynı
ifadeyle geri okuyor. Bu yalnızca aritmetiğin **kendisiyle tutarlı**
olduğunu gösteriyor; `pitch` alanının kullanıldığını göstermiyor.

**Gerçek donanımda.** Satırlarını dolduran bir ekran modu bul — pek
çok gerçek GPU 1366×768'de `pitch`'i 5504 ya da 6144'e yuvarlıyor,
`1366 * 4 = 5464` iken. O makinede:

1. Değiştirilmemiş kodla `fb-test` koş. Beklenen:
   `[fb] pitch N bytes against M bytes of visible pixels: this
   machine pads its rows by N-M` satırı, ve senaryo geçer.
2. Mutasyonu uygula. Beklenen: `verify()` ikinci satırdan itibaren
   kayan adresler yüzünden kırılır — köşe pikselleri ya da
   dikdörtgen kenarları tutmaz.

İkinci adım kırılmazsa testler yetersiz demektir, iddia değil.

**Yanlış çıkarsa.** `pitch` alanı gereksiz olurdu ve `offset` bir
çarpma kazanırdı. Ama Limine onu ayrı bir alan olarak bildiriyor,
ve bir alanın var olması onun bazen farklı olduğunun kanıtı.

---

## 10. Write-combining'in faydası ölçülemiyor (M14)

**İddia** ([ADR 051](adr/051-pat-ve-write-combining.md)): PAT girdisi
4 write-combining'e ayarlandı, ve framebuffer bu tiple eşlendi. WC'nin
kazancı, ardışık yazmaları 64 baytlık patlamalar hâlinde toplamak —
her pikseli ayrı bir bus işlemine çevirmek yerine.

**Ölçüm.** Tam ekran dolduruluyor ve sayılıyor:

```
[fb] full-screen fill: 120541869 cycles for 1024000 pixels
     (117 cycles each), write-combining
```

**Bu sayı bir taban, bir maliyet değil.** QEMU'nun TCG'si bir
write-combining tamponu modellemiyor: her `write_volatile` bir
fonksiyon çağrısına dönüşüyor ve 117 döngünün neredeyse tamamı
emülasyon yükü. Gerçek donanımda beklenen büyüklük mertebesi tek
haneli döngü.

**İkinci bir ölçüm neden yok.** İlk plan, WC'li ve WC'siz iki rakam
raporlamaktı. Vazgeçildi: TCG WC'yi modellemediği için iki rakam
gürültü kadar farklı çıkardı, ve yan yana basılan iki eşit sayı
okuyanı "WC'nin faydası yok" sonucuna davet ederdi. **Anlam
taşıyamayacak bir karşılaştırmayı davet etmek, karşılaştırma
yapmamaktan kötü.**

Ölçümün koruduğu tek iddia şu: WC yolu **çalışıyor** — eşleme
kuruluyor, pikseller doğru yere düşüyor, geri okunuyor.

**Gerçek donanımda.** Aynı dolguyu iki kez koş, biri
`perm::READ_WRITE_COMBINING` biri `perm::READ_WRITE` ile, ve iki TSC
rakamını karşılaştır. Beklenen: WC belirgin biçimde hızlı — bir
framebuffer'da tipik olarak birkaç kat. Değilse ya PAT girdisi
yazılmamış ya da sayfa tablosu biti yanlış yerde; `cpu::init_pat`
MSR'ı geri okuyor, yani ilk ihtimal boot'ta yakalanır.

**Ek olarak: PAT biti ile PS biti.** ADR 051'in asıl bulgusu ölçüm
değildi: seviye 1'de bit 7 **PAT**, seviye 2–3'te **PS**. `is_huge()`
bunu ayırmıyordu ve yalnızca `walk_lookup`'ın `level == 1 ||`
kısa devresi yüzünden kazara doğru çalışıyordu. Artık `is_huge`
seviyeyi alıyor. Bu, donanım doğrulaması gerektirmiyor — SDM Vol. 3A
Tablo 4-19 yeterli — ama 2 MiB sayfa kullanan ilk kod o kısa devreye
güvenemeyecek.

---

## 11. Klavye scan code seti 1'e bağlı, çünkü BIOS öyle bıraktı (M15)

**İddia** ([ADR 053](adr/053-klavye-surucu-modeli.md)): klavye
sürücüsü scan code **set 1** çözüyor. Set 2 değil — brief set 2
varsayıyordu ve ölçüm yanlışladı.

**Ölçüm.** `scripts/m15-spike/`, 512 baytlık bir boot sector, 0x64'ü
yoklayıp 0x60'tan okuduğu baytları seri porta basıyor. Projenin
kendi QEMU bayraklarıyla:

```
SPIKE-READY 1e 9e 2a 1e 9e aa e0 48 e0 c8 1d 2e ae 9d 38 2d ad b8
```

`a` → `1e 9e`. Set 2 olsaydı `1c` ve break iki baytlı (`f0 1c`)
olurdu. Gelen set 1: i8042'nin çeviri biti açık, ve onu açan
SeaBIOS.

**Neden burada sınanamıyor.** QEMU'da yalnızca bir firmware var ve o
da çeviriyi açıyor. "Çeviri kapalıyken ne olur" sorusunu bu ortamda
sormanın tek yolu denetleyiciyi ilklendirmek — ki kapsam dışı
bırakıldı, çünkü ilklendirmeyi *eklemek* bu maddeyi kapatmaz, sadece
başka bir varsayıma taşır.

**Gerçek donanımda.** Üç makine, üç soru:

1. **UEFI ile boot eden bir makine, CSM kapalı.** Legacy USB desteği
   yoksa i8042 hiç olmayabilir. Beklenen: sürücü 0x64'ten 0xFF okur
   (boş bus) ve tek bir tuş üretmez. Bu durumda `kbd-test` vacuous
   geçmemeli — sürücü "hiç bayt gelmedi" diye raporluyor.
2. **Gerçek PS/2 klavyesi olan eski bir makine.** Beklenen: aynı
   baytlar, aynı davranış.
3. **USB klavye, legacy emülasyon açık.** Beklenen: yine set 1, ama
   firmware'e göre değişebilir.

Her üçünde de sürücünün **tanınmayan bayt sayacı** okunacak. Sıfırdan
büyükse akış beklenenden farklı, ve rakam ne kadarının anlaşılmadığını
söylüyor.

**Yanlış çıkarsa.** Bir makinede set 2 gelirse sürücü her baytı
tanınmaz sayar ve tek karakter basmaz — sessizce yanlış karakter
basmaz, ki bu kasıtlı bir tasarım: tanınmayan bayt atılıyor ve
sayılıyor, tahmin edilmiyor. Çözüm, ilklendirme ekleyip seti açıkça
seçmek olurdu (ADR 053'ün elenen (b) seçeneği).

---

## 12. Gölge tampon kazancı ve WC bellekten okuma maliyeti (M15)

**İddia** ([ADR 054](adr/054-konsol-font-ve-kaydirma.md)): konsol
RAM'de gölge tampon tutuyor ve framebuffer'ı yalnızca yazıyor,
çünkü write-combining bellekten **okumak** patolojik olarak yavaş.
Kaydırma, gölge tamponda `copy_within` + framebuffer'a tek yönlü
blit.

**Neden QEMU'da sınanamıyor.** Madde 10 ile aynı sebep: TCG bir WC
tamponu modellemiyor, ve okuma tarafında da gerçek bir PCIe
gidiş-dönüşü yok. Emülasyonda okuma ile yazma neredeyse aynı
maliyette, yani gölge tamponun kazancı **ölçülemiyor** — hatta
negatif görünebilir, çünkü fazladan bir RAM kopyası var.

Ölçüm yine de raporlanıyor: kaydırma süresi bir taban ve yolun
çalıştığının kanıtı.

**Gerçek donanımda.** İki ölçüm:

1. Kaydırmayı iki yolla koş — gölge tamponlu (bugünkü kod) ve
   doğrudan framebuffer içinde `memmove` ile. Beklenen: gölge
   tamponlu olan belirgin biçimde hızlı, çünkü ikincisi 1 milyon
   piksel **okuyor**. Değilse ADR 054'ün (a) seçeneğini eleme
   gerekçesi yanlıştı.
2. Framebuffer'dan tek bir piksel okumanın maliyetini ölç ve
   yazmayla karşılaştır. ADR 054'ün doğrulama stratejisi — tam ekran
   yerine ~50 piksel örneklemek — bu rakamın büyük olduğu
   varsayımına dayanıyor. Küçükse örnekleme gereksiz ve doğrulama
   genişletilebilir.

**Yanlış çıkarsa.** Gölge tampon 3,9 MiB RAM ve bir kopyalama adımı
demek. Kazanç yoksa ikisi de boşa gidiyor ve doğrudan framebuffer'a
çizmek daha basit olurdu.

---

## 13. I/O APIC yönlendirmesi ISA varsayımına dayanıyor (M15)

**Durum:** açık borç, QEMU'da muhtemelen doğru, gerçek donanımda
doğrulanmadı.

### Ne yapıldı

M15'e kadar bu kernel I/O APIC yönlendirme girdisinin yalnızca
maske bitini yazıyordu. Diğer 63 bit — vektör, teslim kipi, hedef
kipi, polarite, tetikleme kipi, hedef CPU — firmware'in bıraktığı
değerdeydi, ve maskesi kaldırılan böyle bir hat hiçbir yere teslim
etmiyordu. IRQ 1 bu yoldan geçen ilk gerçek cihaz hattı olduğu için
eksiklik beş milestone boyunca görünmedi (ADR 042'nin M15 eki).

`IRQControl.Get` artık girdinin tamamını yazıyor:

| Alan | Değer | Kaynağı |
|---|---|---|
| Vektör | `IRQ_VECTOR_BASE + irq` | Kernel'in kendi sözleşmesi |
| Teslim kipi | Fixed | Tek anlamlı seçenek (SDM 11.5.1) |
| Hedef kipi | Physical | Mantıksal şema yok |
| **Polarite** | **Active high** | **ISA varsayımı** |
| **Tetikleme** | **Edge** | **ISA varsayımı** |
| Hedef | LAPIC 0 | Tek CPU (madde 7) |

Kalın yazılan ikisi varsayım.

### Risk

Polarite ve tetikleme kipinin yetkili kaynağı ACPI'nin MADT
tablosundaki **Interrupt Source Override** girdileri. Bu kernel
MADT okumuyor; Limine RSDP'yi veriyor ama hiçbir ACPI tablosu
ayrıştırılmıyor.

QEMU'nun q35 makinesinde IRQ 1 için tipik olarak override yok —
override'lar genellikle IRQ 0→GSI 2 ve SCI hattı için üretiliyor.
Yani varsayım burada muhtemelen yalnızca işe yarıyor değil, doğru.
Bu borcu **küçültüyor, kaldırmıyor.**

Yanlış olursa iki farklı arıza:

- **Polarite ters:** hat hiç tetiklenmez ya da sürekli tetiklenir.
- **Level-triggered bir hattı edge sanmak:** cihaz hattı indirene
  kadar yeniden tetikleme beklenmediği için ilk kesmeden sonra
  sessizlik; ya da tersi durumda **kesme fırtınası**. İkincisi
  eksik kesmeden daha kötü, çünkü sistem meşgul görünerek durur.

PCI cihazları level-triggered ve active-low; bu kernel bir PCI
cihazına kesme hattı verdiği anda varsayım kesin olarak yanlış
olacak.

### Nasıl doğrulanır

1. **Gerçek donanım, legacy PS/2 klavyeli eski bir makine.**
   Beklenen: `kbd-test` geçer ve `[kbd] the I/O APIC has IRQ 1 on
   vector 34, unmasked, fixed delivery to LAPIC 0, edge triggered
   and active high` satırı çıkar. Çıkmazsa `ioapic::verify` hangi
   alanın farklı olduğunu söyler.

2. **MADT'yi elle oku ve karşılaştır.** `acpidump` ile MADT'yi
   dökmek ve Interrupt Source Override girdilerini listelemek
   yeterli. IRQ 1 için bir girdi varsa varsayım o makinede yanlış.

3. **UEFI ile boot eden bir makine.** Madde 11'in senaryosuyla
   aynı koşu; orada scan code seti, burada hat yapılandırması
   sınanıyor.

### Çözüm

MADT okuma. Limine RSDP'yi veriyor, oradan XSDT'ye, oradan MADT'ye;
Interrupt Source Override girdileri GSI numarasını, polariteyi ve
tetikleme kipini bildiriyor.

**M16 veya sonrası, ve bir ADR hak ediyor:** ACPI tablolarına
dokunmak kernel'e yeni bir ayrıştırıcı sokmak demek, ve hangi
tabloların kernel'de hangilerinin bir bileşende okunacağı M15'in
içinde aceleyle verilecek bir karar değil. ADR 046'nın "kernel'de
kalan donanım" listesi bu soruyu da kapsamalı.