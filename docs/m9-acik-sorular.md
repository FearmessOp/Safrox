# M9 architect pass — karara bağlanamayanlar

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

---

## Soru 2 — `Reply`'nin cap taşımasındaki asimetri

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

---

## Soru 3 — Bağlı Notification'ın önceliği ve açlık

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

---

## Soru 4 — Badge 32 bit: ADR 021'i düzeltmek doğru mu?

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

---

## Soru 5 — `NBSend`/`NBRecv` yokluğu M13'ü zorlar mı?

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

---

## Soru 6 — `cancel_ipc`'in O(kuyruk) maliyeti bir DoS mu?

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

---

## Soru 7 — IPC tamponunun ilk 32 baytı israf

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

---

## Özet tablo

| # | Konu | Eğilimim | Ölçülebilir | Etkilediği |
|---|---|---|---|---|
| 1 | Üç word mü dört mü | (b) simetri | hayır | **ABI** |
| 2 | `Reply` cap asimetrisi | (a) böyle kalsın | hayır | güvenlik modeli |
| 3 | Notification önceliği | (c) dönüşümlü | evet, M12'de | davranış |
| 4 | Badge 32 bit | (a) kabul | kısmen | **slot düzeni** |
| 5 | `NBSend` yokluğu | (a)+(c) | M13'te görülür | ABI (geriye uyumlu) |
| 6 | `cancel_ipc` O(kuyruk) | (b) çift yönlü | **evet** | **TCB düzeni** |
| 7 | Tamponun ilk 32 baytı | (b), ölçerek | **evet, kolay** | tampon düzeni |

**En acil olanlar 1, 4 ve 6** — üçü de veri yapısı ya da ABI
değiştiriyor, yani uygulama başlamadan karara bağlanmalı.

3, 5 ve 7 sonradan değiştirilebilir: 3 davranış, 5 yeni bir method
numarası, 7 iç detay.