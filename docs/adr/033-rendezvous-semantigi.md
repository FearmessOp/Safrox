# ADR 033 — Rendezvous: senkron IPC ve bekleyen kuyruğu

Status: accepted (M9 architect pass)

Kaynak: seL4 Reference Manual (v13), bölüm 4.1–4.2.

Bu ADR, IPC'nin en temel sorusunu karara bağlıyor: **bir mesaj
gönderildiğinde ne olur?** Geri kalan yedi ADR'nin hepsi bu cevabın
üstüne kuruluyor.

## Karar: tam senkron randevu, tampon yok

`Send` bir Endpoint'e mesaj koymaz. **Bir alıcıyla buluşur.** Alıcı
hazır değilse gönderen bloke olur; gönderen yoksa alıcı bloke olur.
Mesaj hiçbir zaman "yolda" değildir — ya iki thread aynı anda
randevudadır ve transfer olur, ya da biri bekler.

```
Send(ep, msg):
    alici bekliyorsa:  mesaji ona kopyala, ikisini de calistirilabilir yap
    beklemiyor ise:    gondereni ep'nin kuyruguna koy, bloke et

Recv(ep):
    gonderen bekliyorsa: mesajini kopyala, ikisini de calistirilabilir yap
    beklemiyor ise:      aliciyi ep'nin kuyruguna koy, bloke et
```

### Neden tamponlama yok

Bir Endpoint mesaj tamponlasaydı, tamponun bir boyutu olurdu, ve o
boyut bir kaynak olurdu. O kaynağın da bir sahibi olmalıydı.

Kim? Endpoint'i yaratan mı, gönderen mi, alıcı mı? Hangi cevabı
verirseniz verin, **kernel'in bir bellek havuzu tutması gerekir** — ve
ADR 022'nin kuralı tam da bunu yasaklıyor: kernel kendi adına bellek
ayırmaz.

Alternatif, tamponu gönderenin bütçesinden almak olurdu: her
`Send`'de bir Untyped'dan bir mesaj nesnesi retype etmek. Bu, IPC'yi
bir allocation işlemi yapardı — sıcak yolda, ve ADR 022'nin kesme
bağlamında allocation yasağıyla çatışarak.

Senkron randevunun bedeli açık ve kabul ediliyor: **bir gönderen,
alıcı hazır olana kadar durur.** Bu bir kusur değil, modelin
kendisi — ve M13'ün init sistemi gibi istek-cevap desenli bir
sistemde doğal davranış. Ateşle-unut isteyen `Notification`
kullanır (ADR 038).

### Mesaj nerede duruyor

Gönderenin TCB'sinde. ADR 017 IPC tamponunu TCB'nin içine koydu
(1024 bayt, offset 1024'te) ve bunun sebebi şimdi işliyor: transfer
anında kernel'in iki TCB'si de elinde, ikisi de PMAP üzerinden
erişilebilir, ve hiçbir ek eşleme gerekmiyor.

Endpoint'in kendisi **mesaj tutmuyor** — 32 bayt (ADR 017) ve bu
boyut mesaj sayısıyla büyümemeli.

## Bekleyen kuyruğu: TCB'de bağlantı, Endpoint'te uçlar

Kuyruk, M6'nın hazır kuyruğuyla aynı desende: **intrusive**, TCB'nin
içinde bir `next` alanı, Endpoint'te baş ve kuyruk işaretçileri.

```
Endpoint (32 bayt):
    +0   state: u8      (Idle | SendersWaiting | ReceiversWaiting)
    +8   queue_head: u64  (bekleyen ilk TCB'nin fiziksel adresi, ya da 0)
    +16  queue_tail: u64
    +24  (ayrilmis)
```

### Neden intrusive

ADR 019'un derivation listesi ve M6'nın hazır kuyruğu aynı kararı
verdi, ve gerekçe burada da aynı: **bir `VecDeque` allocate ederdi.**
IPC sıcak yol, ve ADR 022 kesme bağlamında allocation'ı yasaklıyor;
bir Endpoint'e yüz thread sıraya girerse kuyruk yapısının büyümesi
gerekirdi, ve o büyüme bir allocation olurdu.

TCB'de bir `u64` alan: sıfır allocation, sabit maliyet.

### İki kuyruk değil, bir kuyruk ve bir durum

Bir Endpoint'te aynı anda hem gönderen hem alıcı bekleyemez — biri
varsa diğeri gelir gelmez randevu olur ve ikisi de çalışır. Yani
**tek bir kuyruk yeter**, ve `state` alanı kuyruğun ne tuttuğunu
söyler.

Bu, 32 baytlık Endpoint'in nasıl yettiğinin cevabı. İki ayrı kuyruk
dört işaretçi demek olurdu, 32 bayta sığmazdı, ve ADR 017'nin
tavanını aşmak derleme hatası.

### FIFO, öncelik değil

Kuyruk **FIFO**. Bir thread'in önceliği (ADR 024: saklanıyor ama
kullanılmıyor) IPC kuyruğunu etkilemiyor.

Gerekçe iki parçalı:

1. **Öncelikli kuyruğa ekleme sınırlı değil.** Doğru yere eklemek
   için kuyruğu dolaşmak gerekir, ve kuyruk uzunluğu bir bileşenin
   kontrolünde — yüz thread sıraya sokan bir bileşen, her yeni
   `Send`'i yüz adıma mal eder. Master prompt'un "sınırsız döngü
   yok" kuralı bunu yasaklıyor. Sabit öncelik seviyeleri için ayrı
   kuyruklar tutmak çözerdi, ama Endpoint'i 32 bayttan büyütürdü.
2. **M10 gelene kadar öncelik bir sayı.** ADR 024 önceliği "veri,
   politika değil" diye sakladı. IPC'de kullanmak, zamanlayıcıdan
   önce öncelik semantiği icat etmek olurdu.

**Kapattığımız kapı:** öncelik tersine çevirme (priority inversion).
Düşük öncelikli bir thread bir Endpoint'te sıranın başındaysa, yüksek
öncelikli bir thread onun arkasında bekler. Gerçek zamanlı bir
sistemde bu kabul edilemez; bu sistemde M10'da yeniden bakılacak bir
not.

### Kuyruk işlemleri sınırlı mı

| İşlem | Maliyet | Neden sınırlı |
|---|---|---|
| Kuyruğa ekle (sona) | O(1) | `tail` işaretçisi var |
| Kuyruktan al (baştan) | O(1) | `head` işaretçisi var |
| Belirli bir thread'i çıkar | **O(kuyruk uzunluğu)** | tek yönlü liste |

Üçüncüsü sınırlı değil ve bu bilinçli bir kabul. Ne zaman gerekiyor:
bekleyen bir thread suspend edilirse ya da TCB'si revoke edilirse
(ADR 039). Her ikisi de nadir — sıcak yolda değil.

M6'nın `remove_from_queue`'su aynı tercihi yaptı ve aynı gerekçeyle:
çift yönlü liste O(1) yapardı, TCB'ye bir alan daha eklerdi.
**Fark şu ki orada kuyruk uzunluğu thread sayısıyla sınırlıydı;
burada da öyle** — bir Endpoint'te bekleyen her thread'in bir TCB'si
var, ve TCB'ler Untyped'dan geliyor. Sınır, sistemdeki toplam thread
sayısı.

Yine de bir tavan konuyor: kuyruk 1024'ten uzunsa `panic`. M6'nın
`ready_count`'u aynı tavanı kullanıyor, ve gerekçesi aynı: bu
uzunluk bir döngü (cycle) belirtisi, yani bir kernel hatası.

## Randevu anında ne oluyor

Sıra önemli, çünkü her adım bir sonrakinin ön koşulu:

```
1. Mesaji kopyala      (gonderen TCB -> alici TCB)
2. Cap'leri transfer et (varsa; ADR 035)
3. Badge'i yaz          (ADR 037)
4. Alicinin durumunu Ready yap, hazir kuyruguna koy
5. Gonderenin durumunu:
     Send ise   -> Ready (devam eder)
     Call ise   -> BlockedOnReply (ADR 036)
```

**Kopyalama kernel'in içinde, tek yönde.** Gönderen bloke, alıcı
bloke — ikisi de çalışmıyor, yani yarış yok. `InterruptGuard`
gerekmiyor mu? Gerekiyor: bir timer kesmesi araya girip *üçüncü* bir
thread'i çalıştırabilir, ve o thread aynı Endpoint'e `Send`
yapabilir. Transfer bölünemez olmalı.

ADR 025'in kuralı: **transfer boyunca `cli`.** 1 KiB kopyalama, en
kötü durumda. M8'in ölçümü syscall turunu 934 çevrimde gösterdi; 1
KiB kopyalama QEMU'da bunun birkaç katı, gerçek donanımda daha az.
Ölçülecek ve ADR 040'a yazılacak.

## Durum makinesi

M6'nın dört durumuna (ADR 026) iki yeni durum ekleniyor:

```
                     Send/Recv (karsi taraf yok)
         Running --------------------------------> BlockedOnEndpoint
            ^                                              |
            |         randevu olustu                       |
            +----------------------------------------------+

                     Call (randevu oldu, cevap bekleniyor)
         Running --------------------------------> BlockedOnReply
            ^                                              |
            |         Reply geldi                          |
            +----------------------------------------------+
```

| Durum | Anlamı | Kuyrukta mı |
|---|---|---|
| `BlockedOnEndpoint` | Bir Endpoint'te randevu bekliyor | evet, Endpoint'in kuyruğunda |
| `BlockedOnReply` | `Call` yaptı, cevap bekliyor | **hayır** — hiçbir kuyrukta değil |

İkincisinin kuyrukta olmaması ADR 036'nın konusu: cevap hakkı
sunucunun TCB'sinde duruyor, çağıranın değil.

Her iki durum da `is_busy()` açısından **meşgul değil** — yani
ADR 026'nın `ThreadBusy` kuralı bunlara uygulanmıyor. Bloke bir
thread'in TCB'si revoke edilebilir; ne olacağı ADR 039'da.

## Üç invocation

| Çağrı | Haklar | Bloke olur mu |
|---|---|---|
| `Send(ep, msg)` | `WRITE` | alıcı yoksa evet |
| `Recv(ep)` | `READ` | gönderen yoksa evet |
| `Call(ep, msg)` | `WRITE` (+ `GRANTREPLY` cevap için) | her hâlükârda, cevaba kadar |

`NBSend` ve `NBRecv` (bloke olmayan varyantlar) **yok.** seL4'te
varlar ve kullanışlılar, ama her biri yeni bir method numarası ve
yeni bir kod yolu. M13'e kadar gerekmediği gösterilebilir:
ateşle-unut `Notification`, yoklamalı alım ise bir tasarım hatası
işareti. **Kapattığımız kapı:** bir sunucunun "istek var mı" diye
bakıp yoksa başka iş yapması. Açık sorulara yazıldı.

## Elenen alternatifler

**Asenkron IPC (tamponlu).** Yukarıda: tampon bir kaynak, kaynağın
sahibi olmalı, ve kernel bellek ayıramaz.

**Endpoint'te iki ayrı kuyruk (gönderenler / alıcılar).** 32 bayta
sığmaz, ve gerekmiyor: ikisi aynı anda dolu olamaz.

**Çift yönlü kuyruk.** Belirli bir thread'i çıkarmayı O(1) yapardı.
TCB'ye 8 bayt daha, ve o işlem sıcak yolda değil.

**Öncelikli kuyruk.** Ekleme sınırsız olurdu. Sabit seviyeli
kuyruklar Endpoint'i büyütürdü. M10'a ertelendi.

**Mesajı Endpoint'te tutmak.** Endpoint'in boyutunu mesaj boyutuna
bağlardı; ADR 017 tamponun TCB'de olmasına zaten karar vermişti.

## Sonuç

- **Tam senkron randevu.** Tampon yok, mesaj hiçbir zaman yolda
  değil. Gerekçe: tampon bir kaynaktır ve kernel kaynak ayırmaz
  (ADR 022).
- Mesaj gönderenin TCB'sindeki 1 KiB tamponda (ADR 017); Endpoint
  32 bayt ve mesaj tutmuyor.
- Bekleyen kuyruğu **intrusive**: TCB'de `next`, Endpoint'te baş ve
  kuyruk. Allocation yok.
- **Tek kuyruk, bir durum alanı.** Gönderen ve alıcı aynı anda
  bekleyemez, yani iki kuyruğa gerek yok — ve 32 bayta sığmazdı.
- **FIFO.** Öncelikli ekleme sınırsız iş olurdu; öncelik M10'a kadar
  zaten yalnızca saklanan bir sayı (ADR 024). Kapattığımız kapı:
  öncelik tersine çevirme.
- Kuyruk ekleme/çıkarma O(1); **belirli bir thread'i çıkarma
  O(uzunluk)** ve bu kabul ediliyor — yalnızca suspend ve revoke
  yollarında, sıcak yolda değil.
- İki yeni thread durumu: `BlockedOnEndpoint` (kuyrukta),
  `BlockedOnReply` (hiçbir kuyrukta değil). İkisi de `is_busy`
  değil.
- Transfer boyunca kesmeler kapalı (ADR 025); maliyeti ölçülecek
  (ADR 040).
- `NBSend`/`NBRecv` yok. Kapattığımız kapı: yoklamalı sunucu.