# Gerçek donanımda doğrulanacaklar

Bu dosya, **QEMU altında test edilemediği için doğrulanmamış** kalan
iddiaları topluyor. Her madde şunları söylüyor: iddia ne, neden burada
sınanamıyor, gerçek makinede nasıl sınanacak, ve yanlış çıkarsa ne olur.

Bir şeyin bu listede olması "muhtemelen bozuk" demek değil. "Testlerin
geçmesi bu konuda hiçbir şey kanıtlamıyor" demek — ki test takımına
güvenirken bilinmesi gereken bir şey.

Güncelleme: M4.

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
sudo dd if=build/pythos-vmtest.iso of=/dev/sdX bs=4M status=progress
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