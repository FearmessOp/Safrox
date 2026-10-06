# ADR 012 — TLB geçersizleştirme disiplini

Status: accepted (M3 architect pass)

## Bağlam

CPU, sayfa tablosu çevirilerini TLB'de önbellekler. Bir eşlemeyi değiştirip
TLB'yi geçersizleştirmezsek, CPU eski çeviriyi kullanmaya devam eder.
Sonuçları:

- **unmap sonrası stale TLB:** Eşlemeyi kaldırdık, çerçeveyi serbest
  bıraktık, başkasına verildi — ama eski sahip hâlâ erişebiliyor. Sessiz
  bellek bozulması ve M8'den sonra bir güvenlik açığı.
- **izin daraltma sonrası stale TLB:** RW'den RO'ya geçtik, ama eski giriş
  hâlâ yazmaya izin veriyor. W^X garantisi kâğıt üstünde kalır.

Bu hata sınıfının karakteri şu: **hata ile belirti arasında zaman ve mekân
mesafesi var.** Yanlış kod `unmap`'tedir, çöküş çok sonra, tamamen başka bir
yerdedir. Üstelik deterministik değil — TLB girişi tahliye edilmişse hata
görünmez. QEMU ile gerçek donanım arasında da farklı davranır.

M1'de `lateout` olayı bir ders verdi: bir kural yalnızca yorumda yazıyorsa
uygulanmaz. Oradaki kural "bu register'ı sonra okuma"ydı, yorum vardı, yine
de ihlal edildi ve sonuç triple fault oldu. TLB disiplini aynı risk
profilinde, ama belirtisi daha sinsi.

### İzin genişletme ayrı bir durum

x86, bir eşlemeyi **daha izinli** hale getirirken (RO→RW, NX→X) TLB
geçersizleştirmeyi zorunlu kılmaz: işlemci stale bir girişle #PF alır ve
yeniden yürüyüş yapar. Intel SDM Vol. 3A, 4.10.4.3 bunu açıkça söyler.

Buna rağmen **her değişiklikte geçersizleştireceğiz**. Sebep: "bu bir
genişletme mi daraltma mı" sorusunu her çağrı yerinde doğru cevaplamak
gerekirdi, ve yanlış cevabın bedeli sessiz bir hata. Kazanç, nadir bir yolda
birkaç döngü. Takas açık.

## Seçim

**Geçersizleştirme tip sistemiyle zorlanıyor.** `map` ve `unmap`
kullanılması zorunlu bir guard döndürür; guard, flush yapılmadan
düşürülemez.

```rust
/// map()/unmap() bunu döndürür. #[must_use]: kullanılmadan düşürülemez.
///
/// Drop implementasyonu panic eder -- flush'ı atlamak bir kernel bug'ıdır,
/// göz ardı edilebilir bir ihmal değil.
#[must_use = "the TLB must be invalidated before this mapping change takes effect"]
pub struct TlbFlush {
    page: Page,
}

impl TlbFlush {
    /// Bu sayfanın çevirisini geçersizleştir ve guard'ı tüket.
    pub fn flush(self) { /* invlpg + mem::forget(self) */ }

    /// Flush'ı bilerek ertele; çağıran daha geniş bir flush yapacak.
    /// Adı uzun, çünkü kullanımı nadir ve gerekçe istiyor.
    pub fn ignore_because_caller_will_flush_all(self) { /* mem::forget */ }
}

impl Drop for TlbFlush {
    fn drop(&mut self) {
        panic!("mapping changed without invalidating the TLB: {:?}", self.page);
    }
}
```

`Drop`'un panic etmesi sert, ve bilerek. Alternatif — `Drop`'ta sessizce
flush yapmak — çalışırdı ama kuralı görünmez kılardı; `must_use` uyarısı
bastırıldığında kimse fark etmezdi. Panic, ihmali derleme zamanında
(uyarı) ve çalışma zamanında (test) yakalar.

### Kurallar

**1. Tek sayfa değişikliği → `invlpg`.** Tek bir sayfanın çevirisini
geçersizleştirir, gerisini korur.

Referans: Intel SDM Vol. 2A, "INVLPG — Invalidate TLB Entries";
Vol. 3A, 4.10.4.1.

**2. Toplu değişiklik → tam flush.** `mov cr3, cr3` global olmayan tüm
girişleri atar. Eşik: **32 sayfadan fazla**. Bu sayı ölçülmedi, yaygın
pratikten alındı (Linux `tlb_single_page_flush_ceiling` varsayılanı 33) —
M3'te doğrulanıp gerekirse ayarlanacak bir parametre, kutsal bir sabit
değil.

**3. CR3 değişimi → zaten tam flush.** Adres uzayı geçişinde ayrıca bir şey
yapmaya gerek yok. **Ama global sayfalar kalır.**

**4. Global sayfa değiştirilirse → CR3 yetmez.** PMAP ve kernel imajı
`GLOBAL` eşleniyor (adres uzayı değişiminde TLB'de kalsınlar diye, çünkü
her VSpace'te aynılar). Global bir girişi atmanın yolu `invlpg` ya da
CR4.PGE'yi kapatıp açmak. Bu, M3'ten sonra kernel eşlemelerini değiştiren
her kod için bir tuzak; API'de `GLOBAL` bayraklı bir eşlemenin unmap'i ayrı
ele alınacak.

Referans: Intel SDM Vol. 3A, 4.10.4.1 ve 4.10.2.4 ("Global Pages").

**5. Birden fazla CPU → TLB shootdown.** M3 tek CPU. Diğer CPU'lar
geldiğinde, bir eşlemeyi değiştiren CPU diğerlerine IPI göndermek ve
onların flush'ını beklemek zorunda. Bu ADR o mekanizmayı tanımlamıyor ama
yerini ayırıyor: `TlbFlush::flush()` o zaman tek CPU'luk `invlpg`'den
çok-CPU'lu bir protokole dönüşecek. **Guard tipi tam da bu yüzden var:**
o değişiklik tek bir fonksiyonun içinde kalacak, çağrı yerleri
değişmeyecek.

### Sıralama

Geçersizleştirme **sayfa tablosu yazımından sonra** gelir. Ters sıra bir
yarış penceresi açar: flush ile yazım arasında başka bir erişim eski
çeviriyi TLB'ye geri yükleyebilir.

Sayfa tablosu yazımı normal bir bellek yazımıdır; x86'nın bellek modeli
(TSO) yazımları sıralı tutar, ama sayfa yürüyüşü donanımda spekülatif
olabilir. `invlpg` serileştirici değildir; emin olmak için yazımdan sonra
ve flush'tan önce bir `mfence` gerekebilir. **Bu noktayı M3'te QEMU ve
mümkünse gerçek donanımda doğrulayacağım** — SDM'in ifadesi bu konuda
yoruma açık.

## Elenen alternatifler

**Her değişiklikten sonra tam flush.** Basit ve doğru. Ama tek sayfalık bir
değişiklikte tüm TLB'yi atmak, sonraki yüzlerce erişimi sayfa yürüyüşüne
mahkûm eder. M11'de her IPC bir adres uzayı geçişi içerebilir; oradaki
maliyet görünür olur.

**`Drop`'ta sessizce flush yapmak.** Çalışır ve zahmetsiz. Reddedilme
sebebi: kuralı görünmez kılar. Bir gün biri `#[allow(unused_must_use)]`
yazdığında ya da guard'ı bir struct'a koyduğunda, flush'ın ne zaman
olduğu belirsizleşir. Açık `flush()` çağrısı, kodu okuyan kişiye
"burada TLB ile bir iş var" der.

**Flush'ı çağıranın sorumluluğuna bırakmak (guard yok).** En az kod, en çok
risk. Bu projenin en pahalı hata sınıfını tamamen insan dikkatine
bırakmak demek.

**Lazy flush: değişiklikleri biriktirip toplu geçersizleştirmek.** Performans
için cazip, ama arada geçen sürede stale çeviri canlı. Bir unmap'ten sonra
çerçeve serbest bırakılıp yeniden dağıtılırsa pencere gerçek bir hataya
dönüşür. Ancak çerçevenin serbest bırakılmasını flush'a bağlayan bir
mekanizma ile güvenli olurdu — M3 için fazla karmaşık.

**PCID kullanmak.** Adres uzayı geçişinde TLB'yi tamamen atmamayı sağlar;
M11'de IPC maliyetini ciddi düşürebilir. Ama shootdown'ı karmaşıklaştırır
(PCID başına ayrı geçersizleştirme) ve M3'te ölçülebilir bir faydası yok.
Guard tipi bunu ileride eklemeyi engellemiyor.

## Sonuç

- Her eşleme değişikliği bir `TlbFlush` guard'ı üretir; kullanılmadan
  düşürülemez. Unutmak derleme uyarısı, çalışma zamanında panic.
- Tek sayfa `invlpg`; 32 sayfadan fazlası tam flush (eşik M3'te
  doğrulanacak).
- İzin genişletmelerinde de flush yapılıyor — mimari gerektirmiyor, ama
  "genişletme mi daraltma mı" sorusunu her çağrı yerinde sormaktan iyi.
- Global sayfalar CR3 değişiminde atılmaz; kernel eşlemesi değiştiren kod
  bunu açıkça ele almak zorunda.
- Çok-CPU shootdown'ın yeri ayrıldı; guard tipi sayesinde o değişiklik
  çağrı yerlerine yayılmayacak.
- **Doğrulanacak:** sayfa tablosu yazımı ile `invlpg` arasında bellek
  bariyeri gerekip gerekmediği. SDM'in ifadesi yoruma açık; M3'te test
  edilecek.
- Negatif test fikri (M3 kabul testine girecek): bir sayfayı eşle, oku,
  unmap et, **flush etmeden** tekrar okumayı dene. Doğru davranışta #PF
  gelir. Eğer gelmezse stale TLB canlı demektir ve test bunu yakalar.
  Bu testin kendisi `ignore_because_caller_will_flush_all` kullanacak —
  guard'ı atlatmanın tek meşru yolu.