# ADR 034 — Mesaj formatı: dört register, bir tampon, bir etiket

Status: accepted (M9 architect pass)

Kaynak: seL4 Reference Manual (v13), bölüm 4.1 ("Message Registers")
ve Figure 4.1.

ADR 033 randevunun *ne zaman* olduğunu söyledi. Bu ADR *neyin*
taşındığını söylüyor — ve M8'in syscall ABI'si (ADR 030) burada sert
bir kısıt.

## Kısıt: M8 beş register bıraktı

`syscall` RCX'i ve R11'i yok ediyor. ADR 030'un düzeni:

| Register | M8'de taşıdığı |
|---|---|
| RDI | capability CPtr |
| RSI | method numarası |
| RDX | argüman 0 |
| R10 | argüman 1 |
| R8 | argüman 2 |
| R9 | argüman 3 |
| RAX | (dönüşte) sonuç |

IPC için RDI ve RSI zaten harcanmış: hangi Endpoint ve hangi işlem.
Geriye **dört register** kalıyor: RDX, R10, R8, R9.

Bu bir tasarım tercihi değil, bir sayım. Daha fazlası için ya
syscall ABI'sini değiştirmek (M8'i bozmak) ya da RBX/RBP gibi
callee-saved register'ları kullanmak gerekirdi — ki onları da
kaydetmek gerekirdi, ve kazanç kayıpla eşitlenirdi.

## Karar: ilk dört word register'da, gerisi tamponda

```
Mesaj = message tag (RDI) + 0..4 word register + 0..124 word tampon
```

| Taşıyıcı | Kapasite | Maliyet |
|---|---|---|
| RDX, R10, R8, R9 | 4 word (32 bayt) | sıfır — zaten register'da |
| TCB'nin IPC tamponu | 124 word (992 bayt) | kopyalama |

**Toplam en fazla 128 word = 1024 bayt**, ki ADR 017'nin TCB'ye
koyduğu tampon boyutu bu.

### Neden dört, "hepsi tamponda" değil

Dört word'lük bir mesaj — bir istek kodu ve üç argüman — M13'ün
servislerinin çoğu için yeterli. O mesajlar **hiç kopyalanmadan**
taşınıyor: kernel register'ları zaten syscall frame'inde tutuyor
(ADR 030: on beş register da kaydediliyor), ve alıcının frame'ine
yazmak dört `mov`.

Her mesajı tampondan geçirmek, dört word için 1 KiB'lık bir yapıya
dokunmak demek olurdu — iki cache line, iki TCB.

### Neden "hepsi register'da" değil

Dört word 32 bayt. Bir dosya adı, bir yapı, bir tampon adresi
listesi sığmaz. M13'ün init sistemi bileşen başlatırken bir
konfigürasyon geçirecek.

### Tampon neden 124 word, 128 değil

Tampon 1024 bayt = 128 word, ama ilk dört word **kullanılmıyor**.

Sebep: register'daki word'lerle tampondaki word'ler **tek bir
dizinin parçası.** `mr[0]..mr[3]` register'da, `mr[4]..mr[127]`
tamponda. Tampondaki ilk dört slot boş duruyor ki indeksleme
aritmetiği olmasın — `mr[i]`, `i < 4` ise register, değilse
`buffer[i]`.

Alternatif, tamponu `mr[4]`'ten başlatmak ve `buffer[i - 4]`
yazmaktı: dört word kazanır, her erişime bir çıkarma ekler. 32 bayt
için sıcak yolda bir komut; kabul edilmedi.

**Kullanılabilir toplam: 128 word, 1024 bayt.** Tamponun ilk 32
baytı israf, ve bu yazılı.

## Message tag: RDI, 64 bit

Gönderen RDI'da bir *message tag* taşıyor. Alıcı aynı register'da
alıyor.

```
 63                    32 31    21 20 19   16 15            4 3    0
+------------------------+--------+--+-------+----------------+------+
|        label           |reserved|N | caps  |     length     | rsvd |
|        32 bit          | 11 bit |1 | 4 bit |     12 bit     | 4 bit|
+------------------------+--------+--+-------+----------------+------+
                                   ^
                                   +-- bit 20: IS_NOTIFICATION
```

| Alan | Bit | Aralık | Anlamı |
|---|---|---|---|
| `rsvd` | 3:0 | 0 olmalı | Gelecek için; sıfır değilse `InvalidArgument` |
| `length` | 15:4 | 0..128 | Kaç word taşınıyor |
| `caps` | 19:16 | 0..4 | Kaç capability taşınıyor (ADR 035) |
| `IS_NOTIFICATION` | 20 | **kernel yazar** | Uyanış bir Notification'dan geldi (ADR 038) |
| `reserved` | 31:21 | 0 olmalı | Gelecek için |
| `label` | 63:32 | serbest | **Kernel okumuyor.** Uygulamanın işlem kodu |

> **Düzeltme (ADR 038).** Bu alan ilk yazıldığında `reserved` 12
> bitti (31:20). [ADR 038](038-notification-ve-bagli-bekleme.md)
> bağlı bekleme için bir ayırt edici bit gerektirdi — bir thread
> `Recv(endpoint)` yaparken bağlı Notification'ı da onu
> uyandırabiliyor, ve alıcının hangisinin geldiğini bilmesi
> lazım.
>
> Bit 20 bu iş için ayrıldı: **gönderen sıfır yazmak zorunda**
> (kontrol ediliyor), kernel uyandırırken set ediyor. `reserved`
> 11 bite indi.
>
> Alternatif, dönüş değerinde (RAX) ayırt etmekti; reddedildi,
> çünkü RAX hata kodları için ayrılmış (ADR 030) ve "başarılı ama
> farklı" bir değer o kodlamayı karmaşıklaştırır.

### Neden `label` üstte ve 32 bit

Uygulama kodu `label`'ı sürekli kullanacak; sunucu `match
tag.label()` yapacak. Üst 32 bitte olması, çıkarmayı tek bir
`shr rax, 32` yapıyor — maskeleme gerekmiyor.

32 bit, dört milyar işlem kodu. Fazla, ama alternatif 16 bit olurdu
ve o zaman "servis numarası + metod numarası" gibi bir bölüşüm
yapmak isteyen biri sıkışırdı. Kernel okumadığı için bedeli yok.

### Neden iki `reserved` alanı

Bit 3:0 ve 31:20. İkisi de **sıfır olmak zorunda** ve kernel
kontrol ediyor.

ADR 018 aynı şeyi CPtr'da yaptı ve gerekçe aynı: bugün sıfır
tutulan bir alan, yarın bir anlam kazanabilir. Sıfır tutulmazsa
kazanamaz — eski kod çöp bırakır, yeni kod onu bir bayrak sanar.

Alt dörtlü, `length`'i 4-bit hizalı tutmak için de orada: `length`
bitleri 15:4'te, yani `tag & 0xFFF0` doğrudan `length * 16` veriyor.
Kullanılmıyor ama kapıyı açık tutuyor.

### `length` 12 bit, 0..128 aralığında

12 bit 4096'ya kadar sayabilir ama geçerli aralık **0..128**.
Fazlası `MessageTooLong`.

Neden 8 bit değil: 8 bit tam 0..255, ve 128 sınırını kontrol etmeyi
unutmak kolay olurdu — "bir bayta sığıyor, demek ki geçerli". 12 bit
açıkça fazla, yani kontrol açıkça gerekli.

### `caps` 4 bit, 0..4

ADR 035'in konusu. Dört bit 0..15 sayabilir, geçerli aralık 0..4.
Aynı gerekçe.

## Sınırların hepsi kontrol ediliyor

Gönderen tag'i kendi yazıyor ve **hiçbir alanına güvenilmiyor**:

```
tag.rsvd_low         != 0  -> InvalidArgument
tag.rsvd_high        != 0  -> InvalidArgument
tag.IS_NOTIFICATION  != 0  -> InvalidArgument   (yalnizca kernel yazar)
tag.length           > 128 -> MessageTooLong
tag.caps             > 4   -> TooManyCaps
```

Beşi de ADR 030'un "kernel user'a hiç güvenmiyor" kuralının
uzantısı. ADR 023'ün K5'i gereği beşinin de bir testi olacak.

Üçüncüsü özellikle önemli: user kodu `IS_NOTIFICATION` bitini set
edip bir Endpoint mesajı gönderebilseydi, alıcı onu bir
Notification uyanışı sanardı — ve badge yerine maske okurdu.

## Alıcı ne görüyor

Alıcı `Recv`'den döndüğünde:

| Register | İçerik |
|---|---|
| RDI | message tag — ama `length` ve `caps` **kernel tarafından düzeltilmiş** |
| RSI | badge (ADR 037) |
| RDX, R10, R8, R9 | mesajın ilk dört word'ü |
| RAX | hata kodu ya da 0 |

### Neden tag düzeltiliyor

Gönderen 128 word gönderdiğini söyleyebilir ama alıcının tamponu
aynı boyutta — ikisi de 1 KiB, çünkü ikisi de TCB (ADR 017). Yani
kırpma gerekmez.

Gerekmez **ama** cap sayısı değişebilir: gönderenin `GRANT` hakkı
yoksa cap'ler sessizce düşer (ADR 021), ve alıcının gördüğü `caps`
alanı gerçekte kaç cap geldiğini söylemeli. Yalan söyleyen bir tag,
alıcıyı var olmayan bir cap'i aramaya gönderir.

Yani: **`caps` alanı alıcıda, gerçekten transfer edilen sayıyı
gösterir.** `length` olduğu gibi geçiyor.

## Dönüş değeri

ADR 030'un kodlaması aynen geçerli: RAX'ın 63. biti hata bayrağı.
IPC yeni hata değerleri ekliyor:

| Kod | Ne zaman |
|---|---|
| `MessageTooLong` | `length > 128` |
| `TooManyCaps` | `caps > 4` |
| `EndpointDead` | Endpoint revoke edildi, bekleyenler uyandırıldı (ADR 039) |
| `ReplyTargetGone` | Cevap verilecek thread yok (ADR 036) |

## Elenen alternatifler

**Tek bir "mesaj boyutu" alanı, register/tampon ayrımı yok.**
Kernel'in "4'ten küçük mü" diye bakması gerekirdi — ki zaten bakıyor,
ama o zaman gönderen hangi word'ün nerede olduğunu bilemezdi.
Mevcut tasarımda gönderen de biliyor: `mr[0..4]` register.

**Mesaj tamponunu ayrı bir Frame'de tutmak (seL4 böyle yapıyor).**
ADR 017 bunu değerlendirdi ve reddetti: her IPC'de kernel'in o
Frame'e erişmesi gerekir, ve bu ya PMAP üzerinden fiziksel çözümleme
ya da kalıcı bir kernel eşlemesi demek. **Kapattığımız kapı:** 1
KiB'dan büyük mesaj. M13'ün dosya sistemi olmadığı için yeterli.

**`label`'ı kernel'e anlamlı kılmak** (örneğin "bu bir hata cevabı"
biti). Kernel'in uygulama protokolü hakkında bir şey bilmesi
demekti. Mekanizma/politika ayrımının (ADR 011) ihlali.

**Değişken uzunluklu tag.** Küçük mesajlar için bir bayt, büyükler
için daha fazla. Çözümlemeyi dallandırırdı; 64 bit zaten bir
register.

## Sonuç

- **İlk dört word register'da** (RDX, R10, R8, R9), gerisi TCB'nin
  IPC tamponunda. Toplam **128 word = 1024 bayt**.
- Dört sayısı bir tercih değil, M8'in ABI'sinden kalan register
  sayısı.
- Register ve tampon **tek bir dizi**: `mr[0..4]` register,
  `mr[4..128]` tampon. Tamponun ilk 32 baytı kullanılmıyor — indeks
  aritmetiğinden kaçınmak için, ve bu israf yazılı.
- **Message tag RDI'da, 64 bit**: `label` (32, kernel okumuyor),
  `caps` (4), `length` (12), iki `reserved` alanı (sıfır olmak
  zorunda, ADR 018'in kapı açık tutma deseni).
- Her alan sınırı kontrol ediliyor; dördünün de testi olacak
  (ADR 023 K5).
- Alıcının gördüğü `caps` alanı **gerçekten transfer edileni**
  gösteriyor — `GRANT` yoksa cap'ler düşüyor (ADR 021) ve tag
  yalan söylememeli.
- Dört yeni hata kodu: `MessageTooLong`, `TooManyCaps`,
  `EndpointDead`, `ReplyTargetGone`.