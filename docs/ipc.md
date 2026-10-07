# IPC: mesaj formatı, durumlar ve bir alışveriş

Bu belge ADR 033–040'ın birlikte ne anlama geldiğini tek yerde
topluyor. Kararların gerekçesi ADR'lerde; burada **ne olduğu** var.

Güncelleme: **M9 tamamlandı — M9a, M9b ve M9c.** Belge artık
şartname değil, çalışan kodun tarifi.

## Kapsam: ne çalışıyor, ne yazıldı ama kapalı

M9 üç parçada iniyor. Bunun sebebi, on bir kabul kriterinin hepsine
tek seferde girişmenin derlenmeyen ve hiç çalıştırılmamış kod
üretmiş olması — açıkça yazılıyor, çünkü buradaki disiplinin
tamamı "denenmemiş iddia iddia değildir" üzerine kurulu.

| Parça | İçerik | Durum |
|---|---|---|
| **M9a** | Endpoint, `Send`, `Recv`, bekleme kuyruğu, iptal | **çalışıyor, test ediliyor** |
| **M9b** | `Call`, `Reply`, `ReplyRecv`, badge, cevap doğrulama | **çalışıyor, test ediliyor** |
| **M9c** | Capability transferi, Notification, bağlı bekleme | **çalışıyor, test ediliyor** |

`#[allow(dead_code)]` arkasında bekleyen bir şey kalmadı. Üç parça
boyunca o attribute'un yaptığı iş şuydu: yazılmış ama hiç
koşturulmamış kodu derleyici denetiminde tutmak, ve **test
edilmediği halde test edilmiş gibi durmasını engellemek.**

Kabul testleri: `ipc-test`, `ipc-cancel-cost`,
`ipc-endpoint-revoked`, `ipc-call-test`, `ipc-server-dies`,
`ipc-stale-reply`, `ipc-self-call`, `ipc-cost`, `ipc-cap-test`,
`ipc-cap-hostile`, `ipc-notification-revoked`, `ipc-cap-cost`.
Toplam 46 senaryo.

### M9c'nin bulduğu üç kusur — ve hayatta kalan bir mutasyon

M9c'yi açmadan önce ADR 035 ve 038, yazılmış `dead_code` ile
karşılaştırıldı. Üç eksik çıktı, üçü de kod koşmadan:

1. **`release_ipc_references`'ta Notification kolu yoktu.** Bir
   `_ => {}` kolu onu saklıyordu. `Wait`'te bekleyen bir thread,
   Notification revoke edilince sonsuza kadar bekleyecekti —
   `ipc-notification-revoked` başarısız olmaz, **asılırdı**. Kol
   eklendi ve `match` tüketici yapıldı; derleyici o anda
   `IrqHandler`'ı da bildirdi (M12'nin borcu, kaydedildi).
2. **`wait()`'in `_ =>` kolu `Pending`'i yutuyordu.** M9b'nin
   kuralı: her uyandırma yolu bir cevap borçlu, ve "tanımsız" bir
   cevap değil. Yakalayıcı kol, M9b'de üç ayrı kusuru bulan paniği
   susturuyordu.
3. **`recv`'de `pending_signal` sabit `false`'tu.** Bağlı bekleme
   hiç çalışmıyordu; dönüşüm dalı erişilemezdi.

Ve bir **K5 mutasyonu hayatta kaldı** — dönüşüm kaldırıldığında
açlık senaryosu geçmeye devam etti. Sebebi ADR 038'in düzeltme
notunda: bloke bir thread'e giden sinyal teslimatta tüketiliyor,
dolayısıyla bu yapılandırmada bağlı alıcı zaten aç bırakılamıyor.
Dönüşüm uygulandı ve çalışıyor; açlık savı M12'de sınanacak.

### Parçalara bölmek ne kazandırdı

M9b'yi açmak, M9a'nın testlerinin erişemediği **dört** kusur buldu —
her biri ona ulaşan senaryonun ilk koşusunda, ve üçü M9a'nın
gönderip test ettiği kodun içinde:

1. **Global teslimat yuvası sağlam değildi.** "Aynı anda en fazla bir
   teslimat havada olabilir" invariant'ı hiçbir zaman doğru değildi;
   yalnızca erişilemezdi. Uyandırmak çalıştırmak değil — `ReplyRecv`
   ilki hâlâ yuvadayken ikinci bir teslimat park ediyor. Teslimat
   hedef thread'in TCB'sine taşındı (ADR 036).
2. **`Call`'ın acil randevu yolu outcome'ı okumuyordu**, dolayısıyla
   sunucusu ölen bir istemci boş bir mesaj ve bir *başarı* alıyordu.
3. **`send_inner`'ın randevu dalı** uyandırdığı alıcıya outcome
   yazmıyordu. M9a'da görünmezdi, çünkü `recv` sebebi mesajın
   içeriğinden çıkarıyordu — ki ADR 034 boş mesaja izin verdiği için
   o çıkarım da yanlıştı.
4. **Kuyruğa girmiş bir `Call` bir kez uyanıyor**, outcome iki kez
   okunuyordu; ikinci okuma temizlenmiş alanı görüp `Pending`
   buluyordu.

Hepsi birden yazılsaydı hepsi birden gelirdi — hiç çalıştırılmamış
bir derlemede. (Üçünün ayrıntısı ADR 039'da, birincisininki
ADR 036'da.)

### M9a'nın iki taşıyıcı sonucu

- **İptal O(1).** `ipc-cancel-cost` tek bir kuyruk işlemini 50 ve
  500 bekleyenle ölçüyor ve iki rakamın **eşit** olmasını şart
  koşuyor. Tek bir rakam O(1)'i göstermez; ikisinin eşitliği
  gösterir, ve kuyruk tek yönlü olsaydı ikincisi yaklaşık on katı
  çıkardı. Ölçüm 16 örneğin **minimumunu** alıyor — gürültü hep
  ekler, yani en küçük gözlem gerçeğe en yakın olanıdır.
- **Bekleyenler uyandırılıyor.** `ipc-endpoint-revoked` üç thread'i
  bir endpoint'te bloke edip capability'yi siliyor; üçü de
  `EndpointDead` ile uyanmalı. `cancel_ipc` bağlı olmasaydı senaryo
  asılırdı — ki doğru arıza biçimi budur: kimsenin erişemediği
  thread'ler tutan bir kuyruk kibarca raporlanacak bir şey değil.

---

## 1. Syscall ABI'sinin IPC kısmı

M8'in düzeni (ADR 030) değişmiyor; IPC onun üstüne biniyor.

### Çağrıda

| Register | İçerik |
|---|---|
| RDI | **message tag** (Send/Call/Reply) veya CPtr (Recv/Wait) |
| RSI | method numarası |
| RDX | `mr[0]` |
| R10 | `mr[1]` |
| R8 | `mr[2]` |
| R9 | `mr[3]` |
| RCX, R11 | **`syscall` tarafından yok edildi** — kullanılamaz |

Endpoint'in CPtr'ı nerede? **Method numarasının yanında değil** —
RDI tag taşıyor, RSI method taşıyor, ve CPtr için yer kalmadı.

Çözüm: `mr[0]` (RDX) **Send/Call/Reply'de CPtr taşıyor**, ve mesajın
ilk word'ü `mr[1]`'e kayıyor.

```
Send:  RDI=tag  RSI=method  RDX=endpoint_cptr  R10=mr[0]  R8=mr[1]  R9=mr[2]
Recv:  RDI=endpoint_cptr    RSI=method
```

Sonuç: **Send/Call'da register'da üç word, Recv'de dört.** Asimetri
ve ADR 034'ün "dört word" ifadesi burada daralıyor; açık sorulara
yazıldı.

### Dönüşte

| Register | `Recv`/`Call` dönüşü |
|---|---|
| RDI | message tag (`caps` düzeltilmiş, `IS_NOTIFICATION` kernel'den) |
| RSI | **badge** (Endpoint) veya **signalled maskesi** (Notification) |
| RDX | `mr[0]` |
| R10 | `mr[1]` |
| R8 | `mr[2]` |
| R9 | `mr[3]` |
| RAX | 0 veya hata kodu (bit 63 set) |

---

## 2. Message tag: bit bit

```
 63                         32 31    21 20 19   16 15            4 3    0
+-----------------------------+--------+--+------+----------------+------+
|           label             |  rsvd  |N | caps |     length     | rsvd |
|           32 bit            | 11 bit |1 |4 bit |     12 bit     | 4 bit|
+-----------------------------+--------+--+------+----------------+------+
                                        ^
                                        +-- bit 20: IS_NOTIFICATION
```

| Alan | Bit | Yazan | Geçerli aralık | Hata |
|---|---|---|---|---|
| `rsvd` | 3:0 | — | 0 | `InvalidArgument` |
| `length` | 15:4 | gönderen | 0..128 | `MessageTooLong` |
| `caps` | 19:16 | gönderen | 0..4 | `TooManyCaps` |
| `IS_NOTIFICATION` | 20 | **kernel** | gönderen 0 yazmalı | `InvalidArgument` |
| `rsvd` | 31:21 | — | 0 | `InvalidArgument` |
| `label` | 63:32 | gönderen | serbest | — |

**Kernel `label`'ı okumuyor.** Uygulamanın işlem kodu.

### Örnek tag değerleri

```
Veri yok, label 7:
  0x0000_0007_0000_0000

Üç word, cap yok, label 1 ("oku"):
  0x0000_0001_0000_0030
                     ^^
                     length=3 (bit 15:4'te 3 -> 0x30)

Bir word, iki cap, label 2 ("baglan"):
  0x0000_0002_0002_0010
                ^    ^^
                |    length=1
                caps=2

Kernel'den gelen notification uyanışı:
  0x0000_0000_0010_0000
                ^
                bit 20 set, geri kalan sıfır
```

---

## 3. Mesaj word'leri nerede

```
mr[0] .. mr[2]    register'da  (Send/Call: RDX endpoint'e gitti)
mr[0] .. mr[3]    register'da  (Recv dönüşü)
mr[4] .. mr[127]  TCB'nin IPC tamponunda
```

TCB'nin düzeni (ADR 017, offset'ler M8'de ölçüldü):

```
TCB (2048 bayt, 2048'e hizalı)
+------+--------------------------------------------------+
|    0 | Context: rbx rbp r12 r13 r14 r15 rsp rip  (64 B)  |
|   64 | state, priority, stack bounds, switch_count       |
|  136 | vspace_root, vspace_slot, vspace_generation       |
|  ... | M9: reply_to, reply_generation, replying_server,  |
|      |     bound_notification, ipc_next, receive_slots   |
|  256 | FXSAVE alani (512 B, 16-hizali)         [ADR 031] |
|  768 | (ayrilmis, 256 B)                                 |
| 1024 | IPC tamponu (1024 B = 128 word)         [ADR 017] |
|      |   mr[0..4]: KULLANILMIYOR (indeks aritmetigi yok) |
|      |   mr[4..128]: mesaj                               |
+------+--------------------------------------------------+
```

M9'un ekleyeceği alanlar 136–256 arasındaki boşluğa sığıyor:

| Alan | Bayt | ADR |
|---|---|---|
| `reply_to` | 8 | 036 |
| `reply_generation` | 1 | 036 |
| `replying_server` | 8 | 039 |
| `bound_notification` | 8 | 038 |
| `ipc_next` | 8 | 033 |
| `receive_cnode` | 8 | 035 |
| `receive_index` | 8 | 035 |
| `receive_count` | 1 | 035 |
| **toplam** | **50** | 136+50 = 186 < 256 ✓ |

---

## 4. Nesne düzenleri

```
Endpoint (32 bayt)                  Notification (64 bayt)
+----+-------------------+          +----+----------------------+
|  0 | state: u8         |          |  0 | signalled: u32       |
|    |   0 = Idle        |          |  8 | waiter: u64          |
|    |   1 = Senders     |          | 16 | bound_tcb: u64       |
|    |   2 = Receivers   |          | 24 | (ayrilmis, 40 B)     |
|  8 | queue_head: u64   |          +----+----------------------+
| 16 | queue_tail: u64   |
| 24 | (ayrilmis)        |
+----+-------------------+
```

Kuyruk **intrusive**: `queue_head` bir TCB'nin fiziksel adresi, o
TCB'nin `ipc_next` alanı bir sonrakini gösteriyor (ADR 033).

---

## 5. Thread durumları

M6'nın dördüne (ADR 026) M9 iki tane ekliyor:

```
                      Resume                timer / yield
      Suspended --------------> Ready <--------------------> Running
          ^  ^                    ^                           |  |  |
          |  |                    |                           |  |  |
          |  |   Suspend          |                           |  |  |
          |  +--------------------+---------------------------+  |  |
          |                       |                              |  |
          |                       |  randevu olustu              |  |
          |                       +------------------------+     |  |
          |                                                |     |  |
          |  Suspend (cancel_ipc)                          |     |  |
          |  +---------------------------------------------+     |  |
          |  |                                                   |  |
          |  |                    Send/Recv, karsi taraf yok     |  |
          |  |   BlockedOnEndpoint <-----------------------------+  |
          |  |          |  ^                                        |
          |  +----------+  |                                        |
          |                |  Call: randevu oldu                    |
          |                v                                        |
          |         BlockedOnReply <--------------------------------+
          |                |                        Call
          |                | Reply geldi -> Ready
          +----------------+ Suspend / sunucu oldu -> Suspended
```

| Durum | Kuyrukta mı | `is_busy` | Revoke edilebilir |
|---|---|---|---|
| `Inactive` | hayır | hayır | **evet** |
| `Ready` | hazır kuyruğunda | **evet** | hayır |
| `Running` | — | **evet** | hayır |
| `Suspended` | hayır | hayır | **evet** |
| `BlockedOnEndpoint` | Endpoint'in kuyruğunda | hayır | **evet** (ADR 039) |
| `BlockedOnReply` | **hiçbir yerde** | hayır | **evet** (ADR 039) |

`BlockedOnReply` hiçbir kuyrukta değil: onu tutan tek şey
sunucunun `reply_to` alanı, ve geri bağ `replying_server`.

---

## 6. Bir alışveriş, adım adım

Senaryo: bir istemci seri sürücüden bir bayt okuyor.

**Başlangıç durumu.** Sürücü `Recv(ep)` yapmış, bloke. İstemcinin
elinde `ep`'nin `WRITE | GRANTREPLY` haklı, badge'i `0x42` olan bir
kopyası var.

```
Endpoint ep:                    Surucu TCB:
  state = Receivers               state = BlockedOnEndpoint
  queue_head -> surucu            ipc_next = 0
  queue_tail -> surucu            reply_to = 0
```

---

### Adım 1 — İstemci `Call` yapıyor

```
RDI = 0x0000_0001_0000_0010     tag: label=1 ("oku"), length=1
RSI = METHOD_CALL
RDX = ep_cptr
R10 = 0                          mr[0]: hangi port
syscall
```

### Adım 2 — Kernel CPtr'ı çözüyor

`cptr::resolve_typed(space, ep_cptr, Endpoint)` — ADR 018'in
döngüsüz yolu. Haklar kontrol ediliyor: `WRITE` var ✓.

Tag doğrulanıyor: `rsvd` sıfır ✓, `length=1 ≤ 128` ✓, `caps=0` ✓,
bit 20 sıfır ✓.

### Adım 3 — Randevu

`ep.state == Receivers`, yani bir alıcı bekliyor. Kuyruktan
çıkarılıyor:

```
surucu = ep.queue_head
ep.queue_head = surucu.ipc_next   (0)
ep.queue_tail = 0
ep.state = Idle
```

### Adım 4 — Transfer (kesmeler kapalı, ADR 025)

```
surucu.mr[0] = istemci.mr[0]     (register'dan register'a)
badge        = 0x42               (cap'ten, ADR 037)
caps         = 0                  (tasinan yok)
```

### Adım 5 — Cevap hakkı kuruluyor (ADR 036)

```
surucu.reply_to          = istemci_tcb_adresi
surucu.reply_generation  = istemci_tcb_slot.generation
istemci.replying_server  = surucu_tcb_adresi      (ADR 039)
```

**Randevu anında yazılıyor**, `Recv` dönerken değil — yoksa ikinci
bir alıcı araya girip çağıranı çalabilirdi.

### Adım 6 — Durumlar

```
surucu.state  = Ready     -> hazir kuyruguna
istemci.state = BlockedOnReply   -> hicbir kuyruga degil
```

Kesmeler açılıyor. Scheduler sürücüyü çalıştırıyor.

### Adım 7 — Sürücü uyanıyor

```
RDI = 0x0000_0001_0000_0010     tag, degismeden
RSI = 0x42                       badge: hangi istemci
R10 = 0                          mr[0]: port
RAX = 0
```

Sürücü `badge == 0x42` görüyor ve isteğin hangi istemciden geldiğini
biliyor. **İstemci yalan söyleyemez** — badge'i kernel taşıdı.

### Adım 8 — Sürücü işini yapıyor ve cevap veriyor

```
RDI = 0x0000_0000_0000_0010     tag: label=0 (basarili), length=1
RSI = METHOD_REPLY
R10 = 0x41                       mr[0]: okunan bayt ('A')
syscall
```

### Adım 9 — Kernel `reply_to`'yu doğruluyor (üç karşılaştırma)

```
reply_to != 0                            ✓
o TCB'nin slot generation'i esliyor      ✓
o TCB'nin durumu == BlockedOnReply       ✓
```

Üçü de tutuyorsa transfer; biri tutmazsa `ReplyTargetGone` ve
sürücü çökmüyor.

### Adım 10 — Cevap transferi

```
istemci.mr[0] = 0x41
istemci.state = Ready
surucu.reply_to = 0
istemci.replying_server = 0
```

### Adım 11 — İstemci dönüyor

```
RDI = 0x0000_0000_0000_0010
R10 = 0x41
RAX = 0
```

İstemci `'A'` aldı. **Toplam iki syscall** (istemcinin `Call`'ı,
sürücünün `Reply`'si) artı iki bağlam değiştirme.

Sürücü `ReplyRecv` kullansaydı aynı sayı — ama sürücü döngüsünde
bir sonraki `Recv` bedava gelirdi.

---

## 7. Hata kodları

M8'in kodlamasına (ADR 030, bit 63 = hata) eklenenler:

| Kod | Ne zaman | Kim alır |
|---|---|---|
| `MessageTooLong` | `length > 128` | gönderen |
| `TooManyCaps` | `caps > 4` | gönderen |
| `Cancelled` | Bloke thread suspend edildi | bloke olan |
| `EndpointDead` | Endpoint/Notification revoke edildi | bekleyenler |
| `ReplyTargetGone` | Cevap hedefi yok ya da geçersiz | sunucu |
| `NotificationBusy` | İkinci bekleyen | ikinci thread |
| `AlreadyBound` | `BindNotification` tekrarı | bağlayan |

Hepsi `Err` — **hiçbiri panic değil**. ADR 030'un kuralı: kernel
user'ın hatası yüzünden durmaz.

Her birinin bir testi olacak (ADR 023, K5: her `Err` yolu
tetiklenmiş olmalı).

---

## 8. Ölçüm (M9b'de yapıldı)

ADR 040 fastpath'i erteledi ve karşılığında ölçüm sözü verdi.
`ipc-cost` o sözü ödüyor; üç yolu 32 örneğin **minimumuyla**
ölçüyor (gürültü hep ekler, en küçük gözlem gerçeğe en yakın
olandır — M9a'nın iptal ölçümünün zor yoldan öğrettiği şey).

| Yol | Çevrim | Ne içeriyor |
|---|---|---|
| `Send`+`Recv` | 882 | bir randevu, bir bağlam değişimi |
| `Call`+`Reply` | 13 902 | iki randevu, iki bağlam değişimi |
| `ReplyRecv` döngüsü | 13 776 | aynısı, sunucu yarısı tek girişte |

Karşılaştırma tabanı M8'in syscall turu (**934 çevrim**) ve bağlam
değişimi (**~1000**).

**`Call` bir randevudan 13 020 çevrim pahalı.** Beklenen yön bu — o
bir değil iki randevu — ve test bunu bir sağlık kontrolü olarak
*şart koşuyor*: eşit çıksalardı rakamlar işi değil başka bir şeyi
ölçüyor olurdu.

**`ReplyRecv` tasarrufu yalnızca 126 çevrim**, ADR 036'nın
dayandığı ~934'lük kernel girişinin çok altında. Sebep mekanizmada
değil, ölçümün koştuğu yerde: M13'e kadar user mode yok, dolayısıyla
`reply_recv` bir fonksiyon çağrısıyla erişiliyor ve hiçbir ayrıcalık
geçişi tasarruf edilmiyor; tasarruf edilen bir çağrı ve bir endpoint
yeniden çözümlemesi. Bu rakam **rapor ediliyor, iddia edilmiyor** —
ADR 036'nın savı M13'te sınanabilir hale geliyor ve orada sınanmalı.
Yine de kayda geçiyor, çünkü o karşılaştırmanın taban çizgisi bu.

Senaryo bunu bir `assert` değil, iki kollu bir rapor olarak yazıyor:
fark bu ölçümün gürültü tabanına yakın, dolayısıyla döngünün *pahalı*
çıkması da bir arıza değil. Arıza olan, rakamın yorumsuz geçmesi
olurdu.

Henüz ölçülmeyen: **128 word + 4 cap**. Capability transferi M9c'de,
ve ADR 034'ün "ilk word'ler register'da" kararını sınayan senaryo
oraya kalıyor.

Emülasyon şerhi: QEMU'nun TCG'si bir ayrıcalık geçişinin gerçek
silikonda maliyeti olan pipeline boşaltmasını modellemiyor. Her
rakam bir **taban**, ve aralarındaki oranlar tek tek rakamlardan
daha çok şey söylüyor.