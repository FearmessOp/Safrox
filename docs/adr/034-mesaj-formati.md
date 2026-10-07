# ADR 034 — Mesaj formatı: üç register, bir tampon, bir etiket

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

### Dört değil, üç: `Send`'in bir register'ı daha var

Sayım bir adım daha gidiyor. `Send` ve `Call`, hangi Endpoint'e
gittiklerini de söylemeli — ve o CPtr bir register istiyor:

```
Send:  RDI = tag, RSI = method, RDX = endpoint_cptr, R10/R8/R9 = mr[0..3]
Recv:  RDI = tag, RSI = method, RDX = endpoint_cptr, R10/R8/R9 = mr[0..3]
```

Yani **gönderen yönünde üç word.** Alıcı yönünde dört taşınabilirdi
(`Recv` dönerken CPtr'a gerek yok), ama taşınmıyor.

## Karar: her iki yönde **üç** word register'da, gerisi tamponda

```
Mesaj = message tag (RDI) + 0..3 word register + 0..125 word tampon
```

| Taşıyıcı | Kapasite | Maliyet |
|---|---|---|
| R10, R8, R9 | 3 word (24 bayt) | sıfır — zaten register'da |
| TCB'nin IPC tamponu | 125 word (1000 bayt) | kopyalama |

**Toplam en fazla 128 word = 1024 bayt**, ki ADR 017'nin TCB'ye
koyduğu tampon boyutu bu.

> **Düzeltme (M9 uygulama kararı 1).** Bu ADR ilk yazıldığında "ilk
> dört word register'da" diyordu. [docs/ipc.md](../ipc.md) yazılırken
> çelişki çıktı: `Send` için RDX, Endpoint'in CPtr'ını taşıyor, yani
> gönderen yönünde üç word kalıyor — alıcı yönündeyse dört.
>
> **Asimetri reddedildi.** "Kaç word register'da" sorusunun tek bir
> cevabı olmalı; iki cevabı olursa her çağrı yerinde yeniden
> düşünmek gerekir, ve `mr[3]`'ün bazı yollarda var bazılarında yok
> olması tam olarak ADR 023'ün K5'inin bulamayacağı türden bir hata
> üretir — `Reply`'de çalışan bir kod `Send`'de sessizce çöp okur.
>
> Bedeli bir word. Karşılığı, kuralın tek cümlede söylenebilmesi.

### Neden üç, "hepsi tamponda" değil

Üç word'lük bir mesaj — bir istek kodu `label`'da ve üç argüman —
M13'ün servislerinin çoğu için yeterli. O mesajlar **hiç
kopyalanmadan** taşınıyor: kernel register'ları zaten syscall
frame'inde tutuyor (ADR 030: on beş register da kaydediliyor), ve
alıcının frame'ine yazmak üç `mov`.

Her mesajı tampondan geçirmek, üç word için 1 KiB'lık bir yapıya
dokunmak demek olurdu — iki cache line, iki TCB.

`label` da sayılırsa dört bilgi register'da taşınıyor: işlem kodu ve
üç argüman.

### Neden "hepsi register'da" değil

Üç word 24 bayt. Bir dosya adı, bir yapı, bir tampon adresi listesi
sığmaz. M13'ün init sistemi bileşen başlatırken bir konfigürasyon
geçirecek.

### Tamponun ilk üç word'ü: ölçülerek karar verildi

`mr[0]..mr[2]` register'da, `mr[3]..mr[127]` tamponda. Soru şu:
tampondaki `mr[i]`, `buffer[i]` mi `buffer[i - 3]` mü?

| | Kapasite | Maliyet |
|---|---|---|
| `buffer[i]` | 125 word kullanılır, ilk 24 bayt israf | indeks aritmetiği yok |
| `buffer[i - 3]` | 128 word'ün tamamı kullanılır | her erişime bir çıkarma |

Bu ADR ilk yazıldığında birincisi seçilmişti, **ölçülmeden** — ve
ADR 040'ta "ölçmeden optimize etmeyin" yazdığım için bu bir
tutarsızlıktı (açık soru 7).

**Ölçüldü, ve sonuç tahminimin tersi çıktı.** Ayrıntı aşağıda.

## Ölçüm: indeks aritmetiğinin bedeli

Açık soru 7'deki eğilimim **(b)** idi — `buffer[i - 3]` — ve gerekçem
şuydu:

> "Modern derleyiciler `buffer[i - 3]` için bir `lea` üretir ve
> adresleme moduna gömülür — yani *sıfır* ek komut olabilir."

[`scripts/m9-index-probe/`](../../scripts/m9-index-probe/) iki
varyantı kernel'in hedefi ve optimizasyon seviyesiyle derleyip
üretilen assembly'yi karşılaştırıyor. Sonuç:

| Fonksiyon | Komut sayısı |
|---|---|
| `read_wasteful` (`buffer[i]`) | **13** |
| `read_offset` (`buffer[i-3]`) | **17** |
| `copy_wasteful` (`buffer[i]` döngüsü) | **14** |
| `copy_offset` (`buffer[i-3]` döngüsü) | **40** |

### Tek erişim: tahmin kısmen yanlış

```asm
;; read_wasteful -- indeks adresleme moduna gomulu
        cmpq    $3, %rdx
        jb      .LBB3_2
        movq    %rdi, %rsi
        cmpq    $128, %rdx
        jae     .LBB3_3
        movq    (%rsi,%rdx,8), %rax     ; <-- tek komut
        retq

;; read_offset -- cikarma ayri bir komut
        cmpq    $3, %rdx
        jae     .LBB1_1
        leaq    (%rsi,%rdx,8), %rax
        movq    (%rax), %rax
        retq
        addq    $-3, %rdx               ; <-- gomulmedi
        cmpq    $127, %rdx
        ja      .LBB1_5
        leaq    (%rdi,%rdx,8), %rax     ; <-- ve bir lea daha
        movq    (%rax), %rax
        retq
```

Çıkarma adresleme moduna **gömülmedi**. x86-64'ün `base + index*8`
modunda sabit bir yer değiştirme için yer var, ama LLVM burada sınır
kontrolünü (`i - 3 < 128`) ayrı tutmak zorunda kaldığı için indeksi
önce düzeltti.

### Kopyalama döngüsü: tahmin tamamen yanlış, ve asıl mesele bu

```asm
;; copy_wasteful -- tamami bir memcpy'ye indi
        cmpq    $4, %rdx
        jb      .LBB2_1
        cmpq    $128, %rdx
        movl    $128, %eax
        cmovbq  %rdx, %rax
        addq    $24, %rsi
        addq    $24, %rdi
        movl    %eax, %eax
        leaq    -24(,%rax,8), %rdx
        movq    %rdi, %rax
        movq    %rsi, %rdi
        movq    %rax, %rsi
        jmpq    *memcpy@GOTPCREL(%rip)   ; <-- tek cagri

;; copy_offset -- kirk komutluk elle acilmis dongu
        ...
        movq    -24(%rdi,%rdx,8), %r8
        movq    %r8, -24(%rsi,%rdx,8)
        movq    -16(%rdi,%rdx,8), %r8
        movq    %r8, -16(%rsi,%rdx,8)
        ...                              ; 4x unroll + artik dongusu
```

**İki döngü anlamca aynı.** `for i in 3..n { dst[i-3] = src[i-3] }`,
`0..n-3` aralığını kopyalıyor — bir `memcpy`. LLVM bunu görmedi:
indeks ifadesindeki çıkarma, idiom tanıyıcısının aradığı deseni
bozuyor.

Bir insan tabii ki `for j in 0..n-3` yazardı ve o da memcpy'ye
inerdi. Ama bu, bulguyu zayıflatmıyor **güçlendiriyor**: "derleyici
nasılsa görür" varsayımı, tam olarak bu ADR'nin ve ADR 040'ın
uyardığı türden bir varsayımdı, ve yanlış çıktı.

### Karar: **(a)**, `buffer[i]`

Tamponun ilk **üç word'ü (24 bayt) kullanılmıyor**; kullanılabilir
kapasite 125 word. Karşılığında:

- tek erişim 4 komut ucuz,
- **kopyalama döngüsü bir `memcpy` çağrısı** — IPC'nin sıcak yolunda
  26 komut fark.

24 bayt, 1024'ün %2.3'ü. ADR 017'nin tampon boyutu bu kayıpla
birlikte hâlâ 128 word'ü taşıyor (üçü register'da), yani **mesaj
kapasitesi etkilenmiyor** — israf edilen, tamponun kendi içinde
erişilmeyen bir bölge.

> **Bu ölçüm eğilimimi çürüttü**, ve kaydı burada duruyor. Açık soru
> 7'de (b)'yi savunmuştum; ölçüm (a)'yı söyledi. M5'in revoke
> tahmininin 120 kat yanlış çıkması gibi, bu da "ölçülebilir olanı
> tahmin etme" kuralının bir örneği — ama bu sefer tahmin *kodu
> yazmadan önce* yakalandı.

Ölçüm tekrarlanabilir: `sh scripts/m9-index-probe/probe.sh`.

**Kullanılabilir toplam: 128 word, 1024 bayt** (3 register + 125
tampon).

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
| R10, R8, R9 | mesajın ilk üç word'ü |
| RAX | hata kodu ya da 0 |

RDX dönüşte **kullanılmıyor.** Gönderen yönünde Endpoint CPtr'ını
taşıyordu; alıcı yönünde bir word daha taşıyabilirdi ama
taşımıyor — yukarıdaki simetri kararı.

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
Kernel'in "3'ten küçük mü" diye bakması gerekirdi — ki zaten bakıyor,
ama o zaman gönderen hangi word'ün nerede olduğunu bilemezdi.
Mevcut tasarımda gönderen de biliyor: `mr[0..3]` register.

**Asimetrik kapasite (gönderende üç, alıcıda dört).** Bir word
kazandırırdı. Reddedildi: kuralın tek cümlede söylenememesi, o
word'den pahalı.

**Method numarasını `label`'ın içine gömmek.** RSI boşalır, dört word
sığardı. Ama `label`'ın "kernel okumuyor" özelliği bozulurdu — bu
ADR'nin açık bir kararı, ve mekanizma/politika ayrımının (ADR 011)
bir uygulaması.

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

- **İlk üç word register'da** (R10, R8, R9), gerisi TCB'nin IPC
  tamponunda. Toplam **128 word = 1024 bayt**.
- Üç sayısı bir tercih değil, bir sayım: M8'in ABI'sinden dört
  register kalıyor, biri `Send`'de Endpoint CPtr'ına gidiyor.
- **Her iki yönde de üç.** Alıcı yönünde dört taşınabilirdi;
  asimetri reddedildi, çünkü "kaç word register'da" sorusunun tek
  cevabı olmalı.
- Register ve tampon **tek bir dizi**: `mr[0..3]` register,
  `mr[3..128]` tampon, tamponda `buffer[i]` olarak — ilk 24 bayt
  kullanılmıyor.
- Bu **ölçülerek** seçildi ve **eğilimimi çürüttü**: `buffer[i - 3]`
  tek erişimde 4 komut pahalı, ve kopyalama döngüsünde `memcpy`
  tanınmasını bozarak 14 komutu 40'a çıkarıyor. Üretilen assembly
  yukarıda.
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