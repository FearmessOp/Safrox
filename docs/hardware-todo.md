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

## 4. SMEP ve SMAP (M3 → M4'te kısmen kapandı)

**Durum: kısmen çözüldü.** M3'te `-cpu qemu64` bu bitleri bildirmiyordu,
yani kod yolu çalışıyor ama koruma yoktu. M4'te `QEMU_FLAGS` artık
`-cpu qemu64,+smep,+smap` kullanıyor ve boot çıktısı:

```
[cpu] smep: enabled, smap: enabled, pge: enabled
```

`run.py` bu satırı her senaryoda zorunlu tutuyor, yani CPU modelinin
sessizce geri alınması test hatası.

**Hâlâ doğrulanmamış olan.** Bitler CR4'te, ama **koruyacak bir user
sayfası yok.** SMEP'in gerçekten bir user sayfasından komut getirmeyi
engellediği, SMAP'in okuma/yazmayı engellediği M8'den önce
gösterilemiyor. Ayrıca QEMU'nun bu bitleri doğru uyguladığı da
varsayılıyor.

**M8'de.** Bir user sayfası eşle, kernel'den yürütmeyi dene (#PF
bekleniyor, SMEP), sonra okumayı dene (#PF bekleniyor, SMAP), sonra
`stac` ile tekrar oku (başarılı olmalı). Üçü de hem QEMU'da hem gerçek
donanımda.

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