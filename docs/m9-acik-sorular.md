# M9 architect pass — karara bağlanamayanlar

> **Status: closed.** Bu dosya M9 architect pass'inin ara
> çıktısıdır: o anda tek başıma karara bağlayamadığım noktalar,
> seçenekleriyle birlikte. Hepsi cevaplandı ve uygulandı;
> aşağıdaki hiçbir soru açık değil. Dosya silinmedi çünkü
> ADR'lerin kaydetmediği bir şeyi kaydediyor: alternatiflerin
> nasıl tartışıldığı ve hangi gerekçeyle elendiği.
>
> | # | Konu | Karar | ADR |
> |---|---|---|---|
> | 1 | Üç word mü dört mü | **(b)** her iki yönde üç | 034 |
> | 2 | `Reply` cap asimetrisi | **(a)**, gerekçe değişti | 035 |
> | 3 | Notification önceliği | **(c)** dönüşümlü | 038 |
> | 4 | Badge 32 bit | **(a)** + ham `extra()` yasak | 037 |
> | 5 | `NBSend` yokluğu | **(a)+(c)** Notification ile | 033 |
> | 6 | `cancel_ipc` maliyeti | **(b)** çift yönlü | 033 |
> | 7 | Tamponun ilk word'leri | ölçüldü → **(a)** | 034 |
>
> **Eğilimimin değiştiği iki yer, ikisi de gerekçe düzeyinde:**
> soru 2'de savunmanın *türü* (politika değil yapı), soru 3'te
> "ölçmeden ekleme" kuralının *sınırı* (garanti ölçülmez). Üçü
> (1, 6, 7) eğilimimi doğruladı, ikisi (4, 5) kabul edildi.
> Yedincisi — tahminim ölçümde çürüdü, ve o soru en öğretici
> olanı.

Aşağıdaki yedi nokta ya sizin kararınızı gerektiriyor ya da tek
başıma verdiğim kararın gözden geçirilmesini istiyorum. Her biri
için seçenekler ve benim eğilimim yazılı; **M9 uygulamasına
başlamadan önce cevaplanmalı**, çünkü hepsi ya veri yapılarını ya
da ABI'yi etkiliyor.

M5'in listesi altı maddeydi ve ikisi (soru 1 ve 5) uygulamada
yanlış çıktı. Bu sefer hangilerinin ölçülebilir olduğunu da
yazıyorum.

---

## Soru 1 — Register'da üç word mü dört mü? (ABI çelişkisi)

**Sonuç:** (b) seçildi — ADR 034. Her iki yönde üç word register'da,
`mr[3..]` tamponda. "Kaç word register'da" sorusunun tek bir cevabı
oldu.

**Durum:** [ADR 034](adr/034-mesaj-formati.md) "ilk dört word
register'da" dedi. [docs/ipc.md](ipc.md) bölüm 1'i yazarken bir
çelişki çıktı:

`Send`/`Call` için Endpoint'in CPtr'ı da bir register istiyor, ve
RDI (tag) ile RSI (method) zaten dolu. Yani:

```
Send:  RDX = endpoint_cptr, R10/R8/R9 = mr[0..3]   -> UC word
Recv:  RDX/R10/R8/R9 = mr[0..4]                     -> DORT word
```

**Asimetri.** Gönderen üç word yazıyor, alıcı dört word okuyor.

**Seçenekler:**

**(a) Asimetriyi kabul et.** `mr[3]` yalnızca `Reply`'de ve
Notification'da kullanılabilir. Belgelenmiş bir tuhaflık.

**(b) Her iki yönde de üç word.** `mr[3]`'ü hiç kullanma;
register kapasitesi üç, tampon `mr[3]`'ten başlıyor. Simetrik ve
anlaşılır, bir word kaybı.

**(c) Method numarasını tag'in içine koy.** `label`'ın alt
bitlerinden 8 tanesini kernel'e ayır. RSI boşalır, dört word
sığar. Ama `label`'ın "kernel okumuyor" özelliği bozulur —
ADR 034'ün açık bir kararıydı.

**(d) `Recv` için CPtr'ı RDI'da tut** (şu an öyle), `Send` için
tag'i RSI'ya al ve method'u... yine yer yok.

**Eğilimim: (b).** Simetri, bir word'den değerli. "Kaç word
register'da" sorusunun tek bir cevabı olmalı, yoksa her çağrı
yerinde düşünmek gerekir.

**Ölçülebilir mi:** Hayır, bu bir ergonomi kararı. Ama (c)'nin
maliyeti ölçülebilir — `label` maskeleme bir `and` komutu.

**Sorum:** (b) ile sadeleştireyim mi, yoksa dördüncü word o kadar
değerli mi?

### Karar: **(b)** — her iki yönde üç word

Asimetri kabul edilmedi. Gerekçe, eğilimimle aynı: "kaç word
register'da" sorusunun tek bir cevabı olmalı.

Sonradan fark ettiğim bir ayrıntı bu kararı güçlendiriyor:
asimetrik bir ABI'de `mr[3]`'ü okuyan bir kod `Reply` yolunda
çalışır, `Send` yolunda sessizce çöp okur. ADR 023'ün K5'i bunu
yakalayamaz — iki yol da "çalışıyor" görünür. Hata, yalnızca
değerin anlamlı olması gereken bir yerde ortaya çıkar, ve o yer
testten uzakta olabilir.

ADR 034 düzeltildi: başlık, kısıt bölümü, dönüş düzeni, elenen
alternatifler ve sonuç.

---

## Soru 2 — `Reply`'nin cap taşımasındaki asimetri

**Sonuç:** (a) seçildi — ADR 035, ama gerekçe değişti: "varsayılan
güvenli" (bir ayar) yerine "yapısal olarak zorlanamaz" (bir
imkânsızlık). Receive slot açılmamışsa yazılacak yer yok.

**Durum:** [ADR 035](adr/035-capability-transferi.md) bölüm "İki
yönlü mü".

`Call` yapan istemci cap taşımak için `GRANT` hakkı istiyor.
`Reply` yapan sunucu **hiçbir hak istemiyor** — elindeki cap'i
gönderebiliyorsa gönderiyor.

Gerekçem şuydu: reply hakkı zaten istemci tarafından verilmiş
(`GRANTREPLY`), ve o hak "bana cevap ver" demek.

**Rahatsızlığım:** `GRANTREPLY` veren istemci, "bana cevap ver"
derken "bana istediğin cap'i ver" demiş olmuyor. Bir sunucu
istemciye istenmeyen bir cap gönderebilir — örneğin bir Untyped,
ki istemcinin bütçesini karıştırır.

Pratikte zararlı mı? İstemci gelen cap'i reddedebilir
(`SetReceiveSlots` çağırmazsa hiç cap almaz). Yani savunma var.
Ama **varsayılan** cap almak değil almamak olmalı mıydı?

**Seçenekler:**

**(a) Böyle kalsın.** İstemci `SetReceiveSlots` çağırmazsa cap
almaz; varsayılan zaten güvenli (ADR 035).

**(b) `Reply` de `GRANT` istesin.** Simetrik, ama sunucunun
elinde hangi Endpoint cap'i var ki hakkına bakılsın? Reply hakkı
bir cap değil, bir alan (ADR 036). Hangi hakka bakılacağı belirsiz.

**(c) `GRANTREPLY`'ye ikinci bir anlam: "cevapta cap kabul
ederim".** İstemci açıkça izin verir.

**Eğilimim: (a)**, çünkü (b) teknik olarak tanımsız ve (c) bir
hak bitinin anlamını ikiye böler. Ama emin değilim.

**Ölçülebilir mi:** Hayır. Bu bir tehdit modeli sorusu.

**Sorum:** Bir sunucunun istemciye istenmeyen cap göndermesi bir
sorun mu, yoksa `SetReceiveSlots` yeterli savunma mı?

### Karar: **(a)**, ama **gerekçe değişti**

Sonuç aynı, akıl yürütme farklı — ve fark önemli.

Benim gerekçem "`SetReceiveSlots` yeterli savunma" idi. Bu zayıf:
bir savunma *aşılabilir*, ve "yeterli" demek onu ölçmek gerektirir.

Doğru gerekçe: **yetkiyi almak bir eylem.** Cap, alıcının önceden
açtığı bir slot'a düşer. Slot açılmamışsa cap gelmez — sunucunun
elinde onu zorlayacak bir mekanizma *yok.*

| | Ne söylüyor | Nasıl bozulur |
|---|---|---|
| "Varsayılan güvenli" | bir ayar | yanlış kurulabilir, unutulabilir |
| "Yapısal olarak zorlanamaz" | bir imkânsızlık | bozulamaz |

Kodda da farklı görünüyor: bu bir `if count == 0 { drop }` değil,
transfer döngüsünün hiç dönmemesi. Hedef yoksa yazılacak yer de
yok.

Üçüncü bir hak biti (`GRANTREPLY`'ye ikinci anlam) bu yüzden
gereksiz: koruyacağı şey zaten korunuyor.

ADR 035 düzeltildi: "Receive slot ayarlanmamışsa" bölümü, asimetri
bölümü, elenen alternatifler ve sonuç.

---

## Soru 3 — Bağlı Notification'ın önceliği ve açlık

**Sonuç:** (c) seçildi — ADR 038. Dönüşümlü teslim: bir bit, bir
dal. "Ölçmeden ekleme" kuralı optimizasyonlar için, açlık
*garantisi* için değil — ya vardır ya yoktur.

**Durum:** [ADR 038](adr/038-notification-ve-bagli-bekleme.md).

Bir thread hem Endpoint'te bekliyor hem bağlı Notification'ı var.
İkisi aynı anda hazırsa **Notification önce** dedim.

Gerekçe: sinyal zaten birikmiş, teslim etmek randevu gerektirmiyor;
gönderen beklemeye devam edebilir.

**Risk:** sürekli sinyal gelen bir Notification, Endpoint'teki
istekleri aç bırakır. M12'nin sürücüsünde kesme hızı sınırlı
olduğu için pratik değil — **ama bu bir varsayım, ölçüm değil.**

**Seçenekler:**

**(a) Notification önce** (şu anki karar).

**(b) Endpoint önce.** Sinyal birikimli olduğu için kaybolmaz;
bir sonraki `Recv`'de görülür. Ama o `Recv` hiç gelmeyebilir.

**(c) Dönüşümlü.** TCB'de bir bit: son hangisi teslim edildi,
bu sefer diğeri. Açlığı kesin olarak çözer, bir bit ve bir dal
maliyetiyle.

**Eğilimim: (c)**, ama ADR'ye (a) yazdım çünkü (c)'yi ölçmeden
eklemek erken optimizasyon gibi geldi. Şimdi emin değilim —
(c)'nin maliyeti gerçekten küçük.

**Ölçülebilir mi:** **Evet.** M12'nin sürücüsü yazıldığında,
kesme hızını yapay olarak artırıp Endpoint'teki isteklerin ne
kadar beklediği ölçülebilir.

**Sorum:** (c)'yi şimdi mi ekleyelim, yoksa M12'de ölçüp mü karar
verelim? Sonradan eklemek ABI'yi değiştirmiyor, yalnızca
davranışı.

### Karar: **(c) dönüşümlü, şimdi**

Eğilimim doğrulandı, ve ADR'ye (a) yazmamın gerekçesi **kategori
hatası** olarak teşhis edildi:

> Optimizasyon ölçülür, açlık garantisi ölçülmez — ya vardır ya
> yoktur.

ADR 040'ın "ölçmeden ekleme" kuralı optimizasyonlar için yazıldı.
Bir ilerleme garantisine uygulamak, "kilidin gerekli olduğunu
ölçelim" demeye benziyor: veri yarışı ölçümde görünmeyebilir, ve
görünmemesi yokluğunu kanıtlamaz.

İkinci bir hata daha vardı: "M12'de kesme hızı sınırlı" bir
*varsayım*. Sınırı donanım koyuyor — yani bir sürücü hatası ya da
kasıtlı bir kesme fırtınası onu kaldırabilir. ADR 030'un "kernel
user'a güvenmiyor" kuralı burada da geçerli.

Maliyet: bir bit, bir dal.

**Ve açlığın imkânsızlığı test ediliyor** (`ipc-test` 9). Garanti
ölçülmez ama imkânsızlık gösterilebilir: karar geri alınırsa test
geçmez, *asılır.* Sonsuz bekleme, açlığın tanımı — bu yüzden
asılarak başarısız olan bir test, sayı basarak başarılı olandan
daha güçlü bir kanıt.

ADR 038 düzeltildi.

---

## Soru 4 — Badge 32 bit: ADR 021'i düzeltmek doğru mu?

**Sonuç:** (a) seçildi — ADR 037: 32 bit, `extra` alanında. Ham
`extra()` erişimi `pub` olmaktan çıktı; `slot.as_badge()`,
`as_radix()`, `as_size_bits()` ile tipli — yanlış okuma derleme
hatası.

**Durum:** [ADR 037](adr/037-badge.md), ADR 021'in "64 bitlik bir
etiket" ifadesini **32 bite düzeltti.**

Sebep: slot 32 bayt (ADR 017 tavanı) ve badge için ayrı bir alan
yok; mevcut `extra: u32` kullanılıyor.

**Rahatsızlığım iki yönlü:**

1. ADR 021 kabul edilmiş bir karardı ve onu uygulamada değil,
   **başka bir architect pass'te** değiştiriyorum. M5'in revoke
   tahmini uygulamada yanlış çıkmıştı; bu farklı — henüz hiçbir şey
   çalıştırılmadı, yalnızca iki ADR'yi yan yana koyunca çelişki
   göründü.

2. `extra` alanını paylaşmak bir *overloading*: CNode'da radix,
   Untyped'da `size_bits`, Endpoint'te badge. Üç anlam, bir alan.
   Tip bilgisi slot'ta var, yani ayırt edilebilir — ama bir
   `extra()` çağrısının ne döndürdüğü çağrı yerine bakılmadan
   anlaşılmıyor.

**Seçenekler:**

**(a) 32 bit, `extra`'da** (şu anki karar).

**(b) Slot'u 40 bayta çıkar**, badge'e 8 bayt ayır. ADR 017'nin
tavanını deler, her CNode %25 büyür. 256 slotluk bir CNode
8 KiB'dan 10 KiB'a çıkar.

**(c) 32 bit ama ayrı bir alan**, `extra`'yı paylaşmadan. Slot'ta
4 bayt boşluk var mı? ADR 019'un düzeni: 8 (object) + 8
(type/rights/depth/generation) + 16 (derivation) = 32. **Yok.**

**Eğilimim: (a).** Ama ADR 021'i düzeltmek yerine ADR 037'de
"ADR 021'in ifadesi slot ölçüsüyle çelişiyordu" diye not düşmek
daha dürüst olabilir — ki öyle yaptım.

**Ölçülebilir mi:** Kısmen. (b)'nin maliyeti ölçülebilir: CNode
başına bellek. Ama "32 bit yeterli mi" sorusu M13'ün bileşen
sayısına bağlı ve o sayı henüz yok.

**Sorum:** 32 bit badge yeterli mi, ve `extra` paylaşımı kabul
edilebilir mi?

### Karar: **(a)**, ve rahatsızlığımın ikinci yarısına bir çözüm

32 bit kabul. `extra` paylaşımı da kabul — **ama ham `extra()`
çağrısı kod tabanında yasaklanıyor.**

Rahatsızlığımı doğru teşhis etmemiştim. Sorun üç anlamın varlığı
değil; tip bilgisi slot'ta duruyor, yani hangi anlamın geçerli
olduğu her zaman belirli. Sorun, alanın **ayırt edilmeden
okunabilmesi**: bir `slot.extra()` çağrısının ne döndürdüğü çağrı
yerine bakılmadan anlaşılmıyor, ve bir CNode slot'undan badge okuyan
kod derlenir, çalışır, yanlış cevap verir.

Çözüm, alanı bölmek değil **erişimi tiplemek**:

```rust
slot.as_badge()      // Endpoint | Notification degilse panic
slot.as_radix()      // CNode degilse panic
slot.as_size_bits()  // Untyped degilse panic
```

Ham `extra()` `pub` olmaktan çıkıyor. Böylece yanlış okuma bir
**derleme hatası** oluyor — mümkün olan en iyi hata türü, ve
ADR 023'ün "tanımsız durum yok" kuralının bir uygulaması.

Yanlış tipte `panic`, `Err` değil: bu bir kullanıcı hatası değil,
kernel hatası.

ADR 037 düzeltildi (başlık dahil: "64 bit" → "32 bit").

---

## Soru 5 — `NBSend`/`NBRecv` yokluğu M13'ü zorlar mı?

**Sonuç:** (a)+(c) seçildi — ADR 033. Bloke olmayan IPC yok;
sağlık kontrolü Notification ile — zaten doğru desen, `Call`
yanlış soruyu sorar.

**Durum:** [ADR 033](adr/033-rendezvous-semantigi.md) bloke
olmayan varyantları almadı.

Gerekçem: ateşle-unut için `Notification` var, yoklamalı alım bir
tasarım hatası işareti.

**Rahatsızlığım:** M13'ün init sistemi şöyle bir şey isteyebilir:

```
loop {
    her bilesenin saglik durumunu sor   (NBSend?)
    cevap gelmeyenleri isaretle
    uyu
}
```

Bu desende `Call` kullanılamaz — bir bileşen takılırsa init
sistemi de takılır. `Send` + ayrı bir `Recv` thread'i mümkün ama
karmaşık.

**Seçenekler:**

**(a) Yok kalsın.** M13'te gerekirse o zaman eklenir; ABI'yi
genişletmek (yeni method numarası) geriye uyumlu.

**(b) `NBSend`'i şimdi ekle.** Bir method numarası, bir dal:
"alıcı yoksa kuyruğa koyma, `WouldBlock` dön".

**(c) Sağlık kontrolü için `Notification` kullan.** Her bileşen
periyodik olarak bir Notification'a sinyal gönderir; init sistemi
`Poll` ile bakar. Bloke olmayan IPC'ye gerek kalmaz.

**Eğilimim: (a) + (c).** (c) zaten doğru desen — sağlık kontrolü
bir istek-cevap değil, bir sinyal.

**Ölçülebilir mi:** Hayır, ama M13'te *gerekip gerekmediği*
görülür.

**Sorum:** M13'ün tasarımında `NBSend` gerektiren bir desen
öngörüyor musunuz?

### Karar: **(a) + (c)** — `NBSend` yok, sağlık kontrolü Notification ile

Eğilimim onaylandı.

Yazarken fark ettiğim bir şey (c)'yi yalnızca bir kaçınma olmaktan
çıkarıyor: **`Call` ile sorulan sağlık, yanlış soruyu soruyor.**
Cevap veren bir bileşen "çalışıyor" değil, "bu syscall'a cevap
verebiliyor" demektir — asıl iş döngüsü takılmış olabilir ve
`Recv`'de bekleyen thread yine de cevap verir. Periyodik bir
sinyal ise iş döngüsünün *içinden* gelir, yani gerçekten canlılık
ölçer.

Badge'ler bunu doğal kılıyor (ADR 037): her bileşene farklı bir
bit, tek bir `Poll` hepsini birden okur.

M13'te gerekirse eklemek geriye uyumlu: yeni bir method numarası,
mevcut hiçbir yolu değiştirmiyor.

ADR 033 düzeltildi.

---

## Soru 6 — `cancel_ipc`'in O(kuyruk) maliyeti bir DoS mu?

**Sonuç:** (b) seçildi — ADR 033/039. Çift yönlü kuyruk, TCB'ye
`ipc_prev`. İptal O(1); `ipc-cancel-cost` 50 ve 500 thread'de iki
ölçümle kanıtlıyor — tek sayı değil, iki sayının *eşitliği*.

**Durum:** [ADR 039](adr/039-ipc-olum-ve-iptal.md).

Bir Endpoint'te bekleyen bir thread'i çıkarmak, kuyruğu dolaşmayı
gerektiriyor (tek yönlü liste, ADR 033).

Kuyruk uzunluğu **sistemdeki thread sayısıyla sınırlı** — ama o
sayı bir bileşenin kontrolünde olabilir: yeterli Untyped'ı olan bir
bileşen yüzlerce thread yaratıp hepsini bir Endpoint'te
bekletebilir.

Sonra o bileşenin thread'lerinden birini suspend etmek yüzlerce
adım sürer. Kesmeler kapalı (ADR 025).

**Ne kadar kötü:** 1024 thread (ADR 033'ün tavanı) × birkaç komut
= birkaç bin çevrim. M5'in revoke'u 6787 ms ölçüldü; bu onun
yanında hiç. Ama "sınırlı" demek "küçük" demek değil.

**Seçenekler:**

**(a) Kabul et.** Tavan 1024, maliyet birkaç bin çevrim, ve iptal
nadir.

**(b) Çift yönlü kuyruk.** TCB'ye 8 bayt (`ipc_prev`), iptal O(1).
Yer var (soru 4'teki tabloda 50/120 bayt kullanılıyor).

**(c) Endpoint başına kuyruk uzunluğu sınırı.** Yeni bir hata
kodu, yeni bir kontrol.

**Eğilimim: (b).** M6'da çift yönlü kuyruğu reddetmiştim çünkü
"TCB'ye bir alan daha" demişti — ama M9'da zaten altı alan
ekliyorum, yedincisi bedava. Ve burada kuyruk uzunluğu *düşman
kontrolünde*, M6'nın hazır kuyruğundan farklı olarak.

**Ölçülebilir mi:** **Evet.** Bin thread'i bir Endpoint'te
bekletip ortadakini suspend etmek, ve süreyi ölçmek.

**Sorum:** (b)'ye geçeyim mi? ADR 033'ü düzeltmem gerekir.

### Karar: **(b) çift yönlü kuyruk, şimdi**

Eğilimim doğrulandı, ve belirleyici olan kendi yazdığım ayrımmış:
**burada kuyruk uzunluğu düşman kontrolünde**, M6'nın hazır
kuyruğundan farklı olarak.

"Birkaç bin çevrim küçük" argümanı ayrıca reddedildi, ve gerekçe
benim yazdığımdan daha keskin:

> O sayı 1024 tavanına dayanıyor, **ve tavanlar değişir.**

Bir güvenlik özelliğini değişebilir bir sabite bağlamak, tavanı
yükselten bir yamanın sessizce bir DoS açması demek. Maliyetin
kuyruk uzunluğundan *bağımsız* olması, bu bağı tamamen koparıyor.

TCB'ye bir `u64` (`ipc_prev`). M9 zaten birkaç alan ekliyor.

**O(1) ölçülerek kanıtlanacak** (`ipc-cancel-cost`): 50 ve 500
thread'le iki ölçüm, ikisi aynı mertebede. Tek bir sayı bunu
kanıtlamazdı — iki sayının *eşitliği* kanıtlıyor.

ADR 033 düzeltildi: kuyruk bölümü, maliyet tablosu, elenen
alternatifler ve sonuç.

---

## Soru 7 — IPC tamponunun ilk 32 baytı israf

**Sonuç:** (a) seçildi — ADR 034, ama ölçümle: `objdump` ile iki
varyant karşılaştırıldı, `buffer[i-3]` IPC kopya döngüsünü 14
komuttan 40'a çıkardı. Tahminim çürüdü, eğilimim (b) idi, ölçüm
(a)'yı seçti.

**Durum:** [ADR 034](adr/034-mesaj-formati.md).

`mr[0..4]` register'da, `mr[4..128]` tamponda. Tamponun ilk dört
word'ü **kullanılmıyor**, indeks aritmetiğinden kaçınmak için.

32 bayt israf, 1024 baytın %3'ü.

**Rahatsızlığım:** Bu gerçekten bir komut kazandırıyor mu? Modern
derleyiciler `buffer[i - 4]` için bir `lea` üretir ve adresleme
moduna gömülür — yani *sıfır* ek komut olabilir.

Ölçmeden karar verdim, ve ADR 040'ta "ölçmeden optimize etmeyin"
yazdım. Tutarsızlık.

**Seçenekler:**

**(a) Böyle kalsın**, 32 bayt kaybı kabul.

**(b) `buffer[i - 4]` kullan**, tam 128 word kapasite. Üretilen
assembly'ye bak, fark yoksa bu.

**Eğilimim: (b)**, ama **uygulama sırasında ölçülecek** —
derleyicinin ne ürettiğine bakıp karar vermek, şimdi tahmin
etmekten iyi.

**Ölçülebilir mi:** **Evet, ve kolay.** İki varyantı yazıp
`objdump` ile karşılaştırmak.

**Sorum:** Bunu uygulama sırasında ölçüp karar vermek yeterli mi,
yoksa şimdi mi sabitleyelim?

### Karar: ölçüldü, ve sonuç **(a)** — eğilimim çürüdü

Yöntem onaylandı: *"`objdump` ile iki varyantı karşılaştır, fark
yoksa `buffer[i - 3]`."* Ölçüm
[`scripts/m9-index-probe/`](../scripts/m9-index-probe/) ile yapıldı,
kernel'in hedefi ve optimizasyon seviyesiyle.

**Fark var, ve beklediğimin tersi yönde.**

| | Komut |
|---|---|
| `buffer[i]`, tek erişim | 13 |
| `buffer[i-3]`, tek erişim | 17 |
| `buffer[i]`, kopyalama döngüsü | **14** |
| `buffer[i-3]`, kopyalama döngüsü | **40** |

Tahminim şuydu: *"derleyiciler `buffer[i - 3]` için bir `lea` üretir
ve adresleme moduna gömülür — yani sıfır ek komut olabilir."*
Gömülmedi; sınır kontrolü (`i-3 < 128`) indeksin önceden
düzeltilmesini gerektirdi.

**Ama asıl bulgu kopyalama döngüsünde.** `buffer[i]` varyantı bir
`memcpy` çağrısına indi; `buffer[i-3]` varyantı kırk komutluk elle
açılmış bir döngüye. İki döngü anlamca aynı — LLVM'in idiom
tanıyıcısı indeksteki çıkarma yüzünden deseni görmedi.

Ve kopyalama döngüsü **IPC'nin sıcak yolu.** 26 komut fark, 24 bayt
için.

> Bu, "derleyici nasılsa görür" varsayımının yanlış çıkması. ADR
> 040'ın uyardığı tam olarak bu tür bir varsayımdı, ve onu yazarken
> kendim yapmışım. M5'in revoke tahmininin 120 kat sapmasıyla aynı
> aile — farkı şu ki bu sefer **kod yazılmadan önce** yakalandı,
> çünkü ölçüm bir satır komut kadar ucuzdu.

Not: register kapasitesi dörtten üçe indiği için (karar 1) israf 32
değil **24 bayt**, tampon kapasitesi 125 word. Mesaj kapasitesi
etkilenmiyor: 3 register + 125 tampon = 128.

ADR 034'ün "Ölçüm: indeks aritmetiğinin bedeli" bölümünde üretilen
assembly yan yana duruyor. Tekrarlanabilir:
`sh scripts/m9-index-probe/probe.sh`.

---

## Özet tablo

| # | Konu | Eğilimim | Verilen karar | Ölçülebilir | Etkilediği |
|---|---|---|---|---|---|
| 1 | Üç word mü dört mü | (b) simetri | **(b)** ✓ | hayır | **ABI** |
| 2 | `Reply` cap asimetrisi | (a) böyle kalsın | **(a)**, gerekçe değişti | hayır | güvenlik modeli |
| 3 | Notification önceliği | (c) dönüşümlü | **(c)**, şimdi ✓ | evet, M12'de | davranış |
| 4 | Badge 32 bit | (a) kabul | **(a)** + `extra()` yasak | kısmen | **slot düzeni** |
| 5 | `NBSend` yokluğu | (a)+(c) | **(a)+(c)** ✓ | M13'te görülür | ABI (geriye uyumlu) |
| 6 | `cancel_ipc` O(kuyruk) | (b) çift yönlü | **(b)**, şimdi ✓ | **evet** | **TCB düzeni** |
| 7 | Tamponun ilk word'leri | (b), ölçerek | ölçüldü → **(a)** ✗ | **evet, kolay** | tampon düzeni |

**Bir eğilimim çürüdü, iki gerekçe değişti.** Gerekçeler sonuçlardan
daha önemli, çünkü bir sonraki kararda kullanılacak olan onlar:

- **Soru 2:** "varsayılan güvenli" ≠ "yapısal olarak zorlanamaz".
  Birincisi bir ayar, ikincisi bir imkânsızlık; ayarlar yanlış
  kurulabilir.
- **Soru 3:** ADR 040'ın "ölçmeden ekleme" kuralının sınırı.
  Optimizasyon ölçülür, **garanti ölçülmez** — ya vardır ya yoktur.
- **Soru 7:** kuralın kendisi bana uygulandı. "Derleyici nasılsa
  görür" dedim, ölçtüm, görmemiş. Üstelik yanıldığım yer tek
  erişim değil **kopyalama döngüsü** — yani IPC'nin sıcak yolu.

Soru 7'nin çürütülmesi listenin en öğretici maddesi: *ölçülebilir ve
kolay* diye işaretlediğim bir soruyu yine de tahminle cevaplamışım.
Ölçüm bir satır komuttu.