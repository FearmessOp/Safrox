# ADR 039 — Ölüm ve iptal: bloke bir thread'in başına gelebilecekler

Status: accepted (M9 architect pass). **ADR 026'nın M9'a bıraktığı
soruyu kapatıyor.**

Kaynak: seL4 Reference Manual (v13), bölüm 4.3 ("Errors") ve
`cancelIPC` semantiği.

ADR 026 şunu yazmıştı:

> **M9'da yeniden ele alınacak:** `Blocked` durumu, Endpoint
> kuyruğundan çıkarma gerektiriyor.

Bu ADR o işi yapıyor. Master prompt'un 4. kuralı (crash-only) her
durumu tanımlı kılmayı gerektiriyor — "tanımsız" kabul edilmez.

## Durum tablosu: kim neyi revoke edebilir

ADR 026'nın tablosu iki yeni satırla genişliyor:

| Durum | Anlamı | TCB revoke edilebilir mi |
|---|---|---|
| `Inactive` | Hiç başlatılmamış | **evet** |
| `Ready` | Hazır kuyruğunda | hayır — `ThreadBusy` |
| `Running` | CPU'da | hayır — `ThreadBusy` |
| `Suspended` | Kuyruktan çıkarılmış | **evet** |
| `BlockedOnEndpoint` | Endpoint kuyruğunda bekliyor | **evet** (ADR 033) |
| `BlockedOnReply` | Cevap bekliyor, kuyrukta değil | **evet** (ADR 036) |

### Neden bloke durumlar revoke edilebilir

ADR 026'nın `ThreadBusy` gerekçesi şuydu: "scheduler bir referans
tutuyor" — hazır kuyruğunda ya da CPU'da.

Bloke bir thread'i **scheduler tutmuyor.** Onu tutan ya bir
Endpoint kuyruğu ya da bir `reply_to` alanı, ve ikisi de
*temizlenebilir*.

Yani bloke durumlar `Suspended`'a daha yakın: thread duruyor, ve
duran bir thread'i yok etmek güvenli — yeter ki onu tutan yapı
önce temizlensin.

## İptal (`cancel_ipc`): her ölüm yolunun ilk adımı

Bir thread bloke durumdan çıkarılacaksa, önce onu tutan yapıdan
koparılmalı. Tek bir işlem, üç durumu ele alıyor:

```
cancel_ipc(tcb):
    match tcb.state:
        BlockedOnEndpoint ->
            tcb'yi endpoint'in kuyrugundan cikar       (O(kuyruk))
            endpoint bosaldiysa state = Idle
        BlockedOnReply ->
            sunucuyu bul (tcb.replying_server)
            sunucu.reply_to = 0
        diger ->
            hicbir sey
    tcb.state = Suspended
```

### `BlockedOnReply` için ters işaretçi gerekiyor

ADR 036 cevap hakkını sunucunun TCB'sinde tuttu (`reply_to`).
Çağıranı iptal etmek için sunucuyu bulmak gerekiyor — ama
çağıranda sunucuya işaretçi yok.

**Karar: çağıranın TCB'sine `replying_server: u64` ekleniyor.**

İki yönlü bağ:

```
sunucu.reply_to       -> cagiran
cagiran.replying_server -> sunucu
```

Maliyeti TCB'de 8 bayt (768-1024 arası boş alanda, ADR 017'nin
2048 tavanını aşmıyor). Kazancı: iptal O(1).

Alternatif, bütün TCB'leri tarayıp `reply_to == tcb` olanı bulmaktı
— sınırsız iş, master prompt yasağı.

**Tutarlılık kuralı:** iki alan ya ikisi birden dolu ya ikisi
birden boş. Bir `debug_assert` bunu her ikisine dokunan yerde
kontrol edecek.

### Kuyruktan çıkarma O(1)

> **Düzeltme (M9a).** Bu bölüm ilk yazıldığında "O(kuyruk uzunluğu),
> çünkü iptal nadir bir işlem" diyordu ve ADR 033'ün o zamanki tek
> yönlü listesine dayanıyordu. ADR 033 bu kararı kendi "M9 uygulama
> kararı 6" düzeltmesiyle geri aldı; burası o düzeltmeyle
> hizalanıyor. Eski gerekçenin kusuru şuydu: **"nadir" ile "ucuz"
> aynı şey değil.** Kuyruk uzunluğu burada bir düşmanın thread
> sayısı, ve dolaşma kesmeler kapalıyken yapılıyor.

Liste çift yönlü (`ipc_next`, `ipc_prev`), yani bir thread'i
çıkarmak komşularını birbirine bağlamak demek — dolaşma yok.

Ölçüldü: `ipc-cancel-cost`, 50 ve 500 bekleyenle **aynı** maliyeti
raporluyor. Testin iddiası iki ölçümün oranı üzerine kurulu, mutlak
bir sayı üzerine değil; mutlak sayı koşudan koşuya değişiyor, oran
değişmiyor.

Kuyruk uzunluğu yine de ≤ sistemdeki thread sayısı ve 1024'te
`panic` (ADR 033) — ama bu artık bir maliyet sınırı değil, bir
döngü-tespit koruması.

## Dört senaryo

### 1. Bloke bir thread suspend ediliyor

```
Suspend(tcb)  -- ADR 026: her zaman calisir
```

`cancel_ipc` çağrılıyor, thread `Suspended` oluyor. `Resume`
edilirse IPC çağrısından **hata ile** dönüyor:

| Hangi durumdaydı | Ne dönüyor |
|---|---|
| `BlockedOnEndpoint` (Send) | `Cancelled` |
| `BlockedOnEndpoint` (Recv) | `Cancelled` |
| `BlockedOnReply` | `ReplyTargetGone` |

Üçüncüsü ADR 036'nın DoS çıkış yolu: takılmış bir istemci
kurtarılabiliyor.

### 2. Bloke bir thread'in TCB'si revoke ediliyor

```
revoke(untyped_tcb_geldigi)
```

Revoke, TCB'yi silmeden önce `cancel_ipc` çağırıyor. Sonra TCB'nin
belleği Untyped'a dönüyor.

Ek bir adım daha gerekiyor: ölen thread bir **sunucu** ise,
`reply_to` alanı dolu olabilir. O zaman çağıranı uyandırmak gerek:

```
tcb_oluyor(tcb):
    cancel_ipc(tcb)                    -- kendi bloke durumunu temizle
    if tcb.reply_to != 0:              -- bu thread birine cevap borcluydu
        cagiran = tcb.reply_to
        cagiran.replying_server = 0
        cagirani uyandir, ReplyTargetGone ile
    if tcb.bound_notification != 0:    -- ADR 038
        ntfn.bound_tcb = 0
```

Üç temizlik, hepsi O(1) ya da O(kuyruk). **ADR 036'nın "sunucu
çökerse" vaadi burada ödeniyor.**

### 3. Endpoint revoke ediliyor

```
revoke(untyped_endpoint_geldigi)
```

Endpoint'in kuyruğundaki **her thread** uyandırılıyor ve
`EndpointDead` alıyor.

```
endpoint_oluyor(ep):
    while ep.queue_head != 0:
        tcb = kuyruktan cikar
        tcb.state = Ready
        tcb'nin IPC cagrisi EndpointDead donecek
        hazir kuyruguna koy
    ep.state = Idle
```

Döngü **sınırlı**: kuyruk uzunluğu kadar, ve her adım O(1). Kuyruk
uzunluğu sistemdeki thread sayısıyla sınırlı.

#### Neden hepsi uyandırılıyor, reddedilmiyor

Alternatif, ADR 026'nın deseniydi: "kuyruğunda bekleyen varsa
`EndpointBusy` döndür, önce onları temizle".

**Reddedildi**, ve gerekçe ADR 026'dakinden farklı:

Bir TCB'yi suspend etmek, o thread'in **sahibinin** yapabileceği
bir şey — TCB cap'i var. Ama bir Endpoint'te bekleyenler *başka
bileşenlerin* thread'leri olabilir, ve Endpoint'in sahibinin onların
TCB cap'leri yok.

Yani `EndpointBusy` döndürmek, Endpoint'in sahibini **başkalarının
iş birliğine muhtaç** bırakırdı. Bir istemci kasten bekleyerek
Endpoint'in yok edilmesini engelleyebilirdi — bir DoS.

Uyandırmak bu sorunu çözüyor ve maliyeti sınırlı.

### 4. Notification revoke ediliyor

Bekleyen thread (en fazla bir tane, ADR 038) uyandırılıyor,
`EndpointDead` alıyor. Bağlı TCB varsa bağ koparılıyor.

Aynı gerekçe, daha basit — kuyruk yok.

## Bekleyen bir thread'in CSpace'i revoke edilirse

Daha ince bir durum: thread bloke değil, ama **cevap hakkı**
kullanılamaz hale geliyor.

Senaryo: sunucu bir `Call` aldı, `reply_to` dolu. Çağıranın TCB'si
revoke edildi (durum 2), çağıran uyandı — ama *uyanmadan önce*
TCB'si yok oldu.

Bu durumda sunucunun `reply_to` alanı **geçersiz bir adres**
gösteriyor. ADR 036'nın üç karşılaştırması bunu yakalıyor:

```
reply_to != 0
ve generation eslesiyor       <- burada kaliyor
ve durum == BlockedOnReply
```

Yani sunucu `Reply` dediğinde `ReplyTargetGone` alıyor. **Sessiz
bir bozulma değil, bir hata kodu.**

Durum 2'nin temizliği bu alanı zaten sıfırlıyor, yani bu bir
*ikinci savunma hattı*. ADR 028'in VSpace için kurduğu desenin
aynısı: temizlik yapılıyor **ve** geçerlilik kontrol ediliyor.

## Yarış koşulları

Bütün bu işlemler `InterruptGuard` altında (ADR 025). Sebep:

Bir thread'i kuyruktan çıkarırken timer kesmesi gelse, scheduler
yarım çıkarılmış bir kuyruğu dolaşabilir. `remove_from_queue`
sırasında `head`, `tail` ve bir `next` alanı tutarsız olabiliyor.

M6'nın `remove_from_queue`'su aynı kararı verdi.

## `suspend` ve `block_current` neden iki ayrı işlem

Bu bölüm M9a'nın gerçeklemesi sırasında eklendi. ADR'nin asıl
pass'inde yoktu, çünkü sorun ancak kod yazılınca göründü.

### Hata

`recv`'in ilk sürümü şunu yapıyordu:

```
recv(ep):
    kendini ep'nin kuyruguna ekle
    state = BlockedOnEndpoint
    sched::suspend(kendisi)      <- YANLIS
```

Üç satır da tek tek doğru görünüyor. Beraberce yaptıkları şu:
`suspend`, yukarıdaki satır 1015'te, thread'in durumunun
`BlockedOnEndpoint` olduğunu görüyor ve **bu ADR'nin emrettiği şeyi**
yapıyor — `cancel_ipc` çağırıyor. Yani bir satır önce kuyruğa
eklenen alıcı, kuyruktan çıkarılıyor.

Sonuç: endpoint tekrar boş. Ardından gelen gönderici de bekleyen
kimseyi bulamayıp bloke oluyor. İki thread birbirini bekliyor,
hiçbiri kuyrukta değil.

### Belirtinin neden hiçbir şey söylemediği

**Hiçbir şey bozulmadı.** Sarkan işaretçi yok, tutarsız kuyruk yok,
`debug_assert` tetiklenmedi. İki işlem birbirini tam olarak
götürdü — kuyruk, hiç dokunulmamış haliyle, geçerli.

Bu yüzden tek belirti otuz saniyelik bir zaman aşımıydı, ve zaman
aşımı hiçbir satır numarası vermez. Bu kod tabanındaki en kötü
teşhis türü: bozulma, kendi yokluğundan ayırt edilemiyor.

İlk tahminim `InterruptGuard`'lardı — yani koda bakmadan verilmiş
bir cevap. Asıl neden koddaydı ve okunabilirdi.

### Kök neden: iki farklı işlem, tek isim

| | `suspend` | `block_current` |
|---|---|---|
| Kim istiyor | başkası, TCB cap'i ile | thread'in kendisi |
| Anlamı | "beklemeyi bırak" | "bekliyorum, gelince uyandır" |
| IPC kuyrukları | **çıkarır** (`cancel_ipc`) | **içinde bırakır** |
| Thread durumu | `Suspended` yapar | çağıranın bıraktığı gibi bırakır |
| Kim çağırır | `Suspend` invocation, `revoke` | `send`, `recv` |

Satırların hepsi zıt. Aynı isim altında toplanmaları, "ikisi de
thread'i durduruyor" benzerliğindendi — ve bu benzerlik tuzağın
kendisi.

**Bu, bu projede üçüncü kez görülen desen:**

- M3: `map` içindeki örtük TLB geçersizleştirmesi → `TlbFlush` ayrı
  bir tip oldu ([ADR 012](012-tlb-gecersizlestirme.md)).
- M7: user belleğine düz okuma → `UserAccess` ayrı bir tip oldu.
- M9a: `suspend` → `block_current` ayrıldı.

Üçünde de kural aynı: **birbirine benzeyen ama farklı şey ifade
eden iki işlem aynı ismi paylaşmamalı.**

### İki taraflı koruma

Ayrım, yalnızca iki fonksiyon yazarak değil, her birinin yanlış
kullanımını *isimlendirerek* kuruldu:

```
suspend:        assert!(!(is_current && state.is_blocked_on_ipc()))
                "bir thread kendini IPC beklemesinden suspend edemez;
                 sched::block_current() kullanin"

block_current:  debug_assert!(state.is_blocked_on_ipc())
                "cagiran once kuyruga eklemeli ve durumu ayarlamali,
                 yoksa bu thread'i hicbir sey uyandiramaz"
```

İkisi birbirinin aynası: biri "bu işlem burada yanlış" diyor, diğeri
"bu işlemin ön koşulu tutmuyor". Her iki yanlış kullanım da artık
otuz saniyelik sessizlik yerine bir satır numarası üretiyor.

`suspend` tarafındaki `assert!`, `debug_assert!` değil — release
derlemesinde de kalıyor. Gerekçe: bu hata sessizce bir kilitlenme
üretiyor, yani en pahalı başarısızlık biçimi, ve kontrol zaten sıcak
yolda değil.

### `block_current`'ın kilitlenmeyi isimlendirmesi

Çalıştırılacak thread kalmadığında `block_current` panik ediyor:

> every thread is blocked on IPC and there is nothing left to run.
> This is a deadlock -- some component is waiting for a message that
> only a blocked thread could send

Sistemin asılı kalmasına izin verilmiyor. Master prompt'un crash-only
kuralı: **bir kilitlenme bir teşhistir, zaman aşımı değildir.**

## Hata kodları

| Kod | Ne zaman | Kim alır |
|---|---|---|
| `Cancelled` | Bloke thread suspend edildi | bloke olan |
| `EndpointDead` | Endpoint/Notification revoke edildi | bekleyenler |
| `ReplyTargetGone` | Cevap verilecek thread yok | sunucu, `Reply`'de |
| `ThreadBusy` | `Ready`/`Running` thread revoke denemesi | revoke eden |
| `NotificationBusy` | İkinci bekleyen (ADR 038) | ikinci thread |

Beşinin de ADR 023 K5 gereği bir testi olacak: **her `Err` yolu
tetiklenebilir olmalı.**

## Elenen alternatifler

**Bloke thread'leri de `ThreadBusy` ile reddetmek.** Endpoint'te
bekleyen bir thread, sahibi tarafından suspend edilebilir — yani
zaten bir çıkış yolu var. Reddetmek bir adım daha eklerdi, kazanç
yok.

**Endpoint revoke'unda `EndpointBusy`.** Bekleyenler başka
bileşenlerin olabilir; Endpoint'in sahibi onları suspend edemez.
Bir DoS vektörü.

**`replying_server` yerine TCB taraması.** Sınırsız iş.

**Ölen sunucunun çağıranını uyandırmamak.** Çağıran sonsuza kadar
bloke kalırdı, ve bu sefer `Suspend` dışında çıkış yolu olmazdı —
oysa bu durum kernel'in bilebileceği bir durum.

**Zombie TCB (seL4'ün çözümü).** Kademeli yıkım, bir durum daha,
ve kapsamdan büyük. ADR 026 bunu zaten reddetmişti.

## Sonuç

- **ADR 026'nın M9'a bıraktığı soru kapandı.** `BlockedOnEndpoint`
  ve `BlockedOnReply` durumundaki thread'lerin TCB'si **revoke
  edilebilir** — scheduler onları tutmuyor.
- Her ölüm yolunun ilk adımı `cancel_ipc`: kuyruktan çıkar ya da
  `reply_to` bağını kopar, sonra `Suspended`.
- Çağıranın TCB'sine **`replying_server`** eklendi — iki yönlü bağ,
  iptali O(1) yapıyor. Alternatif TCB taramasıydı, yani sınırsız iş.
- **Ölen bir sunucunun çağıranı uyandırılıyor** (`ReplyTargetGone`).
  ADR 036'nın vaadi burada ödeniyor.
- **Endpoint revoke'unda bekleyenlerin hepsi uyandırılıyor**
  (`EndpointDead`), reddedilmiyor. Gerekçe ADR 026'dan farklı:
  bekleyenler başka bileşenlerin thread'leri olabilir ve Endpoint'in
  sahibi onları suspend edemez — reddetmek bir DoS vektörü olurdu.
- Temizlik **ve** geçerlilik kontrolü birlikte: `reply_to` sıfırlanıyor
  *ve* `Reply` üç karşılaştırma yapıyor (ADR 028'in deseni).
- Bütün iptal işlemleri `InterruptGuard` altında (ADR 025).
- Beş hata kodu, her birinin testi olacak (ADR 023 K5).