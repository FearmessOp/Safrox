# ADR 038 — Notification ve bağlı bekleme: sürücünün iki kaynağı

Status: accepted (M9 architect pass)

Kaynak: seL4 Reference Manual (v13), bölüm 5 ("Notifications") ve
5.3 ("Binding Notifications to TCBs").

ADR 017 Notification'ı kümeye aldı ve "asenkron sinyal" dedi. Bu ADR
semantiğini ve **M13'ün sürücüsünün ihtiyaç duyacağı soruyu**
cevaplıyor: bir thread aynı anda hem Endpoint'te hem
Notification'da bekleyebilir mi?

## Notification nedir

Bir **32 bitlik maske** ve bir bekleyen thread.

```
Notification (64 bayt, ADR 017):
    +0   signalled: u32    -- birikmis sinyal bitleri
    +8   waiter: u64       -- bekleyen TCB'nin fiziksel adresi, ya da 0
    +16  bound_tcb: u64    -- bagli TCB (asagida), ya da 0
    +24  (ayrilmis, 40 bayt)
```

Endpoint'ten üç farkı var ve her biri bilinçli:

| | Endpoint | Notification |
|---|---|---|
| Gönderen bloke olur mu | evet (randevu) | **hayır** |
| Sinyaller birikir mi | hayır (tek randevu) | **evet** (OR'lanır) |
| Kaç bekleyen | kuyruk | **bir tane** |

### `Signal` bloke olmuyor

```
Signal(ntfn_cap):
    badge = ntfn_cap.badge          (ADR 037)
    ntfn.signalled |= badge
    bekleyen varsa:
        ona signalled'i ver, signalled = 0, uyandir
```

Gönderen hiçbir durumda beklemiyor. Bu, Notification'ın var olma
sebebi: **bir kesme işleyicisi bloke olamaz.**

M12'nin IRQHandler'ı bir Notification'a bağlanacak, ve kesme geldiğinde
kernel `Signal` çağıracak. O noktada kernel kesme bağlamında ve
bloke olması imkânsız.

### Sinyaller OR'lanıyor

Beş `Signal` gelse, beş badge OR'lanır ve bekleyen thread hepsini
bir kerede görür.

Sayaç değil, **maske**. "Üç kere sinyal geldi" bilgisi kaybolur;
"şu üç kaynaktan geldi" bilgisi korunur.

Gerekçe: sayaç taşabilir ve taşınca ne olacağı bir karar daha
gerektirir. Maske taşmaz — aynı bit iki kere set edilirse yine set.

**Kapattığımız kapı:** "kaç tane" sorusu. Bir üretici-tüketici
kuyruğunda "kaç öğe eklendi" sayılamaz. Çözüm: paylaşılan bellekte
bir sayaç, Notification yalnızca "bak" demek için.

### Tek bekleyen

Bir Notification'da **en fazla bir thread** bekleyebilir. İkincisi
`NotificationBusy` alır.

Endpoint'teki gibi kuyruk yok, çünkü anlamı olmazdı: sinyal
birikimli, yani ikinci bekleyen de aynı maskeyi görecekti. "Kim
alacak" sorusunun iyi bir cevabı yok.

seL4 de böyle yapıyor.

## Bağlı bekleme (bound notification)

**Soru:** bir thread hem Endpoint'te hem Notification'da bekleyebilir
mi?

**Cevap: evet, `BindNotification` ile.**

M13'ün sürücüsü tam olarak buna ihtiyaç duyuyor:

```
sürücü döngüsü:
    bekle:
        bir istemci istegi geldi   (Endpoint)
        ya da bir kesme geldi       (Notification)
    hangisi geldiyse onu isle
```

İkisi için ayrı thread kullanmak mümkün ama o zaman sürücünün
durumu iki thread arasında paylaşılır, ve kilitleme gerekir — tek
CPU'da bile, çünkü preemption var.

### Nasıl çalışıyor

```
BindNotification(tcb_cap, ntfn_cap)
```

TCB'ye bir `bound_notification` alanı, Notification'a bir
`bound_tcb` alanı yazıyor. İkisi birbirini gösteriyor.

Bundan sonra o thread `Recv(endpoint)` yaptığında **iki şeyden
biri** onu uyandırır:

1. Endpoint'te bir randevu olur → normal IPC, badge RSI'da
2. Bağlı Notification sinyal alır → `Recv` döner, ama **farklı
   bir şekilde**

### Hangisi geldiğini nasıl ayırt ediyor

Message tag'in (ADR 034) `label` alanı kernel tarafından
yazılmıyor — ama bir istisna gerekiyor burada.

**Karar: `length = 0`, `caps = 0` ve RSI'da Notification maskesi.**

Bir Notification uyanışında:

```
RDI = 0              (tag tamamen sifir: length 0, caps 0, label 0)
RSI = signalled maskesi
RAX = 0
```

Bir IPC uyanışında `label` sıfır olabilir ama tag'in tamamı sıfır
olmaz — en azından gönderen bir `length` yazmıştır... ama
yazmayabilir de. Sıfır uzunluklu, sıfır label'lı bir mesaj geçerli.

**Bu bir belirsizlik ve çözülmesi gerekiyor.** İki seçenek:

**(a) Tag'de bir "bu bir notification" biti.** `reserved` alanından
bir bit harcanır (ADR 034'te bit 31:20 arası boş). Kernel yazar,
gönderen yazamaz (sıfır olmak zorunda, kontrol ediliyor).

**(b) RAX'ta farklı bir dönüş değeri.** `Recv` 0 yerine 1 döner.

**Karar: (a), bit 20 = `IS_NOTIFICATION`.**

Gerekçe: RAX hata kodları için ayrılmış (ADR 030) ve "başarılı ama
farklı" bir değer o kodlamayı karmaşıklaştırır. Tag'de bir bit,
alıcının zaten okuduğu bir register'da.

ADR 034'ün `reserved` alanı 12 bitti; artık 11 bit (31:21) ve bir
bit kullanımda. ADR 034 buna göre güncellenecek.

### Bağlı Notification'ın önceliği: **dönüşümlü**

İkisi aynı anda hazırsa — Endpoint'te bir gönderen *ve* bağlı
Notification'da bir sinyal — hangisi?

**Dönüşümlü (alternating).** TCB'de bir bit: en son hangisi teslim
edildi. Bu sefer diğeri.

```
Recv(endpoint) uyandirilirken, ikisi de hazirsa:
    son teslim Notification ise  -> Endpoint'i teslim et
    son teslim Endpoint ise      -> Notification'i teslim et
    bit'i cevir
```

Yalnızca biri hazırsa bit'e bakılmıyor: hazır olan teslim edilir ve
bit ona göre güncellenir.

> **Düzeltme (M9 uygulama kararı 3).** Bu ADR ilk yazıldığında
> **"Notification önce"** diyordu, ve açlık riskini bir açık soru
> olarak bırakıyordu: "M12'nin sürücüsünde kesme hızı sınırlı olduğu
> için pratik değil."
>
> Açık soru 3'te eğilimimi (c) dönüşümlü olarak yazmış ama ADR'ye
> (a)'yı koymuştum — gerekçem, "ölçmeden eklemek erken optimizasyon
> gibi geldi" idi.
>
> **Bu gerekçe kategori hatasıydı.** Dönüşümlü teslim bir
> optimizasyon değil, bir **doğruluk garantisi.** Ve ikisi farklı
> şekilde doğrulanır:
>
> | | Nasıl doğrulanır |
> |---|---|
> | Optimizasyon | **ölçülür** — ne kadar hızlandı |
> | Açlık garantisi | **ölçülmez** — ya vardır ya yoktur |
>
> ADR 040'ın "ölçmeden ekleme" kuralı birincisi için yazıldı.
> İkincisine uygulamak, "kilidin gerekli olduğunu ölçelim" demeye
> benziyor: veri yarışı ölçümde görünmeyebilir, ve görünmemesi
> yokluğunu kanıtlamaz.
>
> "M12'de kesme hızı sınırlı" bir varsayımdı. Sınırı kim koyuyor?
> Donanım — yani bir sürücü hatası, kötü yapılandırılmış bir aygıt,
> ya da kasıtlı bir kesme fırtınası o sınırı kaldırabilir. Bir
> ilerleme garantisini donanımın iyi davranmasına bağlamak, ADR
> 030'un "kernel user'a güvenmiyor" kuralının ihlali.
>
> Maliyeti **bir bit ve bir dal.**

### Açlığın imkânsızlığı test ediliyor

Garanti ölçülmez, ama **imkânsızlık gösterilebilir.** `ipc-cap-test`
senaryo 9:

```
Notification'a kesintisiz sinyal gonderilirken
Endpoint'e de surekli bir gonderen kuyrukta duruyor.
Iki endpoint teslimati arasindaki EN UZUN bildirim serisini bas.
```

Ölçülen şeyin bir **seri uzunluğu** olması gerekiyor, tek bir
gecikme değil: açlık "bir kez yavaş kaldı" değil, "hep başkasının
arkasında" demek.

> **Düzeltme (M9c uygulaması).** Bu bölüm "karar geri alınırsa test
> *asılır*" diyordu. **Yanlış çıktı, ve K5 mutasyonu bunu gösterdi.**
>
> Dönüşüm kaldırılıp yerine "hep Notification" konduğunda senaryo
> **geçti**: 8 endpoint mesajı teslim edildi, en uzun seri 3, sınır
> olan 4'ün altında. Sinyal akışı turda bir sinyalden beş sinyallik
> bir seriye çıkarıldığında rakam hiç değişmedi (48 sinyal, aynı
> seri).
>
> Sebebi, karar anının izi alınınca göründü ve akışın zayıflığı
> değil, tasarımın bir özelliği:
>
> ```
> sender_waiting=1 pending_signal=1 last_was_ntfn=1 -> notification
> sender_waiting=1 pending_signal=0 last_was_ntfn=1 -> endpoint
> ```
>
> Her çevrimin ikinci turunda maske **boş**. `Signal`, *bloke* bir
> bağlı thread'e bitleri doğrudan verip temizliyor; o thread `recv`'e
> ulaştığında bekleyen bir şey kalmıyor ve endpoint **çekişmesiz**
> kazanıyor. Dönüşüm biti o turda hiç okunmuyor, dolayısıyla
> kaldırılması bir şeyi değiştirmiyor.
>
> **Sonuç, garantinin test edilmediği değil: bu yapılandırmada bağlı
> bir alıcı zaten aç bırakılamıyor.** Bloke bir thread'e giden sinyal
> teslimatta tüketiliyor, yani ikinci bir sinyal endpoint'in önüne
> geçemiyor. Bu, dönüşümün verdiğinden daha güçlü bir özellik ve bit
> olsun olmasın geçerli.
>
> Endpoint'i aç bırakmak için maskeyi **alıcının bloke olduğu
> pencere boyunca** dolu tutan bir kaynak gerekiyor — yani test
> thread'i çalışırken sinyal gönderen *başka* bir thread. Tasarımda
> böyle tek bir kaynak var: M12'nin kesme işleyicisi.
>
> Durum, açıkça:
>
> - dönüşüm **uygulandı** ve **çalıştırılıyor** (senaryo, çekişmeli
>   karar sayısının sıfır olmadığını iddia ediyor; seçimi yapan o
>   daldır);
> - açlık savı **başarısız olan bir testle gösterilmedi**;
> - gösterecek yapılandırma **M12'de** doğuyor, ve iddia orada
>   sınanmalı.
>
> Hayatta kalan bir K5 mutasyonu, test hakkında bir sonuçtur. Bunu
> gömmek, dosyadaki diğer bütün K5 iddialarının değerini düşürürdü.
>
> Eski cümlenin nereden geldiği de kayda değer: yazıldığında makul
> bir tahmindi ve hiç koşulmamıştı. "Test asılır" demek, "test
> asıldı" demekle aynı şey değil.

### Bir thread kaç Notification'a bağlanabilir

**Bir tane.** TCB'de tek bir alan.

İkisi gerekiyorsa: iki Notification'ı tek bir Notification'a
yönlendirmek mümkün değil (Notification'lar birbirine sinyal
gönderemez), ama **badge'ler farklı olabilir** — aynı
Notification'a iki farklı badge'li cap, iki farklı kaynak.

Bu zaten ADR 037'nin Notification badge'ini maske yapma sebebi.

### Bağlı Notification ve `Signal` doğrudan `Wait`

Bir thread bağlı Notification'ında doğrudan da bekleyebilir:
`Wait(ntfn)`. O zaman Endpoint devreye girmiyor.

İki yol da var çünkü sürücü iki fazda çalışabilir: başlatma
sırasında yalnızca kesme bekler, çalışma sırasında ikisini birden.

## Notification'ın invocation'ları

| Çağrı | Haklar | Bloke |
|---|---|---|
| `Signal(ntfn)` | `WRITE` | hayır |
| `Wait(ntfn)` | `READ` | sinyal yoksa evet |
| `Poll(ntfn)` | `READ` | **hayır** — maskeyi okur ve sıfırlar |

`Poll` neden var ama `NBRecv` yok (ADR 033): Notification zaten
bloke olmayan bir mekanizma, ve bir sürücünün "kesme geldi mi" diye
bakması meşru bir desen. Endpoint'te aynı şey yoklamalı sunucu
olurdu.

## `BindNotification` ne zaman reddediliyor

```
tcb zaten bagli            -> AlreadyBound
ntfn zaten bagli           -> AlreadyBound
ntfn'de bekleyen var       -> NotificationBusy
tcb bloke durumda          -> ThreadBusy (ADR 026 deseni)
```

Son ikisi ADR 026'nın "meşgul bir şeyi değiştirme" kuralının
uzantısı.

`UnbindNotification` da var, aynı kontrollerle.

## Elenen alternatifler

**Bağlı bekleme olmadan, iki thread.** Sürücünün durumu paylaşılır,
kilitleme gerekir. Tek CPU'da bile preemption var.

**Notification her zaman önce.** Bu ADR'nin ilk kararıydı. Açlık
üretiyor: sürekli sinyal gelen bir Notification, Endpoint'teki
istekleri sonsuza kadar bekletir. Yukarıdaki düzeltme notu.

**Endpoint her zaman önce.** Sinyal birikimli olduğu için kaybolmaz,
bir sonraki `Recv`'de görülür — **ama o `Recv` hiç gelmeyebilir.**
Sürekli istek gelen bir Endpoint bu sefer kesmeleri aç bırakır. Aynı
hata, ters yönde.

**Endpoint ve Notification'ı tek tipte birleştirmek.** ADR 017
reddetti: her IPC'de "bu senkron mu asenkron mu" dalı.

**Notification'da kuyruk.** Sinyal birikimli olduğu için ikinci
bekleyen de aynı maskeyi görürdü; "kim alacak" sorusunun iyi cevabı
yok.

**Sinyal sayacı (maske yerine).** Taşma ne olacak sorusu, ve
taşmanın sessizce bilgi kaybetmesi.

**Dönüş değerinde ayırt etmek (seçenek b).** RAX hata kodları için;
"başarılı ama farklı" o kodlamayı karmaşıklaştırır.

## Sonuç

- Notification bir **32 bitlik maske** ve **tek bir bekleyen**.
  Sinyaller OR'lanıyor, sayılmıyor — sayaç taşması bir karar daha
  gerektirirdi. Kapattığımız kapı: "kaç tane geldi".
- `Signal` **hiçbir zaman bloke olmuyor**; bir kesme işleyicisi
  bloke olamaz ve M12 buna dayanacak.
- **Bağlı bekleme var** (`BindNotification`): bir thread
  `Recv(endpoint)` yaparken bağlı Notification'ı da onu uyandırır.
  M13'ün sürücüsü için gerekli, ve şimdi karara bağlandı.
- Ayırt etme: tag'de **bit 20 = `IS_NOTIFICATION`**, kernel yazar.
  ADR 034'ün `reserved` alanı 12 bitten 11 bite indi.
- İkisi aynı anda hazırsa **dönüşümlü teslim**: TCB'de bir bit, en
  son hangisinin teslim edildiğini tutuyor. Bir bit, bir dal.
- Gerekçe: bu bir optimizasyon değil **doğruluk garantisi**, ve
  garantiler ölçülmez — ya vardır ya yoktur. "Kesme hızı sınırlı"
  varsayımı donanımın iyi davranmasına güvenmek olurdu.
- **Açlık senaryosu var** (`ipc-cap-test` 9) ve dönüşümlü dalın
  çalıştığını gösteriyor: çekişmeli karar sayısı sıfır değil. Ama
  K5 mutasyonu (dönüşümü kaldır) bu senaryoyu **öldürmedi** —
  yukarıdaki düzeltme notu. Bağlı bir alıcı bu yapılandırmada
  zaten aç bırakılamıyor; açlığı gösterecek kaynak M12'nin kesme
  işleyicisi, iddia orada sınanacak. **Açık kalem.**
- Bir thread **bir** Notification'a bağlanabilir; birden çok kaynak
  için badge bitleri kullanılır (ADR 037).
- `Poll` var, `NBRecv` yok: Notification zaten bloke olmayan bir
  mekanizma.