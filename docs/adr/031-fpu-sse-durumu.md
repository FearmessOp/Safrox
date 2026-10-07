# ADR 031 — FPU/SSE durumu: her switch'te kaydet, lazy yapma

Status: accepted (M8)

M6 bu borcu açıkça erteledi: "kernel thread'leri SSE kullanmıyor —
build `-mno-sse` ile geçiyor — yani kaybedilecek bir şey yok. M8'in
user thread'leri bunu zorunlu kılacak." M8 geldi.

## Neden artık kaçınılmaz

Kernel SSE kullanmıyor ve bu bir derleyici bayrağıyla *zorlanıyor*.
User kodu için öyle bir bayrak yok ve olamaz: user kodu bu kernel
tarafından derlenmiyor, ve derlense bile user'a "SSE kullanma"
demenin bir yolu yok — bir capability değil, bir temenni olurdu.

Pratikte x86-64'te SSE'den kaçınmak neredeyse imkânsız: `memcpy`'nin
her makul uygulaması XMM kullanıyor, SysV ABI float argümanları
XMM0-7'de taşıyor, ve derleyiciler tamsayı kodunda bile 16 baytlık
kopyalar için XMM'e düşüyor.

Yani: iki user thread'i varsa ve biri bir XMM register'ına yazıp
preempt edilirse, geri döndüğünde o register'da diğerinin verisi
olur. Bu yalnızca bir doğruluk hatası değil — **bir bilgi sızıntısı.**
Thread A'nın XMM0'ında ne varsa thread B okuyabilir.

## Eager: her switch'te `fxsave`/`fxrstor`

Karar: bağlam değiştirmede FPU/SSE durumu **koşulsuz** kaydediliyor ve
geri yükleniyor. Lazy switching yok.

### Lazy switching ne olurdu

Klasik numara: CR0.TS'i set et, bir thread FPU'ya dokunduğunda `#NM`
(device not available) alınır, handler o anda eski durumu kaydeder ve
yenisini yükler. FPU kullanmayan thread'ler hiç `fxsave` ödemez.

Üç sebeple reddedildi, ve üçüncüsü belirleyici:

**1. Kazanç varsayımsal.** Lazy, "çoğu thread FPU kullanmıyor"
varsayımına dayanıyor. Yukarıda anlatıldığı gibi, modern user kodunda
bu varsayım yanlış. Kazanç `#NM` handler'ının maliyetiyle birlikte
negatife bile düşebilir.

**2. Karmaşıklık kernel'in içinde.** Lazy, "FPU durumu şu anda kimin"
sorusunu kernel'e taşıyor ve bir `FPU_OWNER` değişkeni gerektiriyor.
O değişkenin thread ölümüyle, revoke'la, SMP'yle etkileşimi var. Bir
thread öldüğünde FPU sahibi oysa, durumu nereye kaydedilecek?

**3. Bir güvenlik hatası sınıfı.** Lazy switching, Intel'in
"Lazy FP State Restore" açığının (CVE-2018-3665) temeli. Spekülatif
yürütme, `#NM` tetiklenmeden önce eski sahibin FPU register'larını
okuyabiliyordu. Linux dahil tüm büyük çekirdekler bundan sonra eager'a
geçti.

Bu üçüncü sebep, ilk ikisinin birleşiminden daha ağır: bir
optimizasyonun bilinen bir açık sınıfı olması, onu "ölçüp karar
veririz" kategorisinden çıkarıyor.

### Maliyeti ölçülüyor

`fxsave`/`fxrstor` çifti 512 bayt yazıp okuyor. M7'nin ölçümü aynı
adres uzayında switch başına 2166 çevrim, farklı adres uzaylarında
48173 demişti. FPU'nun eklediği fark testte raporlanıyor — ve
raporlanmasının sebebi, kararın gerekçesinin "ölçmedim ama güvenli"
değil, "ölçtüm ve şu kadar" olması.

## Nerede saklanıyor: TCB'nin ayrılmış alanı

ADR 017 TCB'yi 2048 bayt olarak sabitledi: 1024 bağlam + 1024 IPC
buffer. Bağlam kısmının çoğu boştu — `Context` yalnızca 64 bayt, geri
kalan `_reserved`.

`fxsave` alanı **512 bayt** ve **16 bayta hizalı** olmak zorunda.
Hizalanmamış bir `fxsave` `#GP` verir, ve bu hata ilk FPU kullanan
thread'e kadar görünmez.

TCB'nin kendisi 2048'e hizalı (ADR 017), yani TCB içindeki 16'nın
katı her offset mutlak olarak da 16-hizalı. Alan **offset 256**'ya
yerleştiriliyor:

```text
    0 .. 64     Context (callee-saved + RSP + RIP)
   64 .. 136    scheduler ve vspace alanları
  136 .. 256    ayrılmış
  256 .. 768    FXSAVE alanı (512 bayt, 16-hizalı)
  768 .. 1024   ayrılmış
 1024 .. 2048   IPC buffer (M9)
```

256, 136'dan sonraki ilk "yuvarlak" 16 katı değil (144 olurdu). 256
seçildi çünkü aradaki boşluk, scheduler alanlarının büyümesi için yer
bırakıyor ve offset'in kendisi bir `const` assertion'la korunuyor:
alan IPC buffer'a taşarsa build kırılıyor.

Hizalama de assert ediliyor — çalışma zamanında değil, derleme
zamanında. `offset_of!` ile.

## FXSAVE mi XSAVE mi

`xsave` daha yeni ve AVX durumunu da kaydediyor. **Kullanılmıyor.**

`xsave`'in kaydettiği alan boyutu CPUID'den okunuyor ve değişken —
AVX-512'li bir makinede 2 KiB'yi aşıyor, yani TCB'ye sığmazdı. Sığdırmak
için TCB'nin boyutunu çalışma zamanında belirlemek gerekirdi, ki ADR
017'nin sabit boyut kararını bozar ve retype aritmetiğini karmaşıklaştırır.

`fxsave` sabit 512 bayt: x87, MMX ve XMM0-15. AVX durumu kaydedilmiyor,
yani **AVX kullanan bir user thread'i preempt edilirse YMM'nin üst
yarısını kaybeder.** Bu bilinen bir eksik ve kasten kabul ediliyor:
CR4.OSXSAVE set edilmediği için user kodu AVX'i *kullanamaz* —
`xgetbv`/AVX komutları `#UD` verir. Yani kaybedilecek durum yok.

Koşul bu: OSXSAVE kapalı kaldığı sürece `fxsave` yeterli. AVX
açıldığında `xsave`'e geçmek gerekecek ve TCB boyutu sorusu o zaman
açılacak. Bilinen eksiklere yazıldı.

## Hangi CR0/CR4 bitleri

**CR0.EM (bit 2) temiz.** Set olsaydı her SSE komutu `#UD` verirdi.
Limine temiz bırakıyor ama kontrol ediliyor — "muhtemelen temiz" bir
şey üzerine FPU kurmak M3'ün NXE dersine aykırı.

**CR0.MP (bit 1) set.** `fwait`/`wait` komutlarının TS ile birlikte
doğru davranması için. TS kullanılmıyor (lazy yok), ama MP'yi set
etmek mimarinin önerdiği yapılandırma ve maliyeti yok.

**CR0.TS (bit 3) temiz, ve temiz kalıyor.** Lazy switching'in anahtarı;
kullanılmadığı için hiç set edilmiyor. Bunu *açıkça* temizlemek,
"TS set unutulmuş olabilir" olasılığını kapatıyor.

**CR4.OSFXSR (bit 9) set.** `fxsave`/`fxrstor`'u ve SSE komutlarını
etkinleştiriyor. Set edilmezse SSE `#UD`.

**CR4.OSXMMEXCPT (bit 10) set.** SSE'nin maskelenmemiş kayan nokta
hatalarını `#XM` (vektör 19) olarak raporlamasını sağlıyor. Set
edilmezse aynı hatalar `#UD` olarak gelir — yani "geçersiz komut",
ki tamamen yanıltıcı. M1'in IDT'si vektör 19'u zaten dolduruyor.

**CR4.OSXSAVE (bit 18) temiz.** Yukarıda; AVX'i kapalı tutuyor.

Hepsi `cpu::init`'te, M3'ün NXE/SMEP/SMAP'iyle aynı desende: CPUID'den
desteği oku, yaz, **geri oku ve doğrula**. Sessizce tutmayan bir bit,
ilk FPU kullanımında anlaşılmaz bir `#UD` demek.

## İlk durum: `fninit` ile temiz

Yeni bir thread'in FXSAVE alanı sıfır değil, **temiz bir FPU durumu**
olmalı. Sıfırlarla dolu bir alanı `fxrstor` etmek:

- MXCSR'ı 0 yapar. Varsayılan 0x1F80 (tüm istisnalar maskeli); 0 ise
  tüm istisnalar *açık* ve ilk kayan nokta işlemi `#XM` verir.
- x87 kontrol word'ünü 0 yapar, ki benzer şekilde yanlış.

MXCSR'ın rezerve bitlerine geçersiz değer yazmak `fxrstor`'u `#GP`
ile de düşürebilir.

Çözüm: retype bir TCB'yi sıfırlarken FXSAVE alanı da sıfırlanıyor, ve
`Tcb::configure` oraya temiz bir durum **yazıyor**: MXCSR = 0x1F80,
x87 CW = 0x037F. Tüm alanı `fninit`+`fxsave` ile üretmek yerine iki
alanı elle yazmak yetiyor, çünkü geri kalan sıfırlar zaten doğru
(boş register'lar, temiz tag word).

Bu, M6'nın "yeni thread'in RBP'si sıfırlanıyor, çünkü başka birinin
RBP'siyle başlayan bir thread'in backtrace'i sahip olmadığı bir
stack'e yürür" kararının aynısı: başlangıç durumu *belirli* olmalı,
"her neyse" olmamalı.

## Test: sayaç switch sayısıyla eşleşiyor

Kabul testi "XMM korunuyor" demekle yetinmiyor. İki şey ölçülüyor:

1. **Değer korunuyor.** Bir user thread'i XMM0'a bir desen yazıyor,
   preempt ediliyor (başka bir thread farklı bir desen yazıyor), geri
   dönünce kendi desenini okuyor.
2. **`fxsave` sayacı switch sayısına eşit.** Lazy switching
   *olmadığının* kanıtı bu. Birinci kontrol, lazy bir uygulamada da
   geçerdi — doğru çalışan bir lazy switcher de değeri korur. Sayaç
   eşitliği, kaydın *her* switch'te yapıldığını gösteriyor.

İkinci kontrol olmadan ADR'nin "lazy yapma" kararı test edilmemiş
olurdu; ADR 023'ün K5'i tam olarak bunu yasaklıyor — bir kararın
kendi yokluğundan ayırt edilemez olması.

### Mutasyon testi: sayacın koruyamadığı şey

Mutasyon denendi — `save_fpu` iki çağrıdan birini atlayacak şekilde
değiştirildi — ve **iki şey öğretti.**

Birinci kontrol (XMM değeri) mutasyonu yakaladı, net bir mesajla:
"thread A's XMM0 reads 0xfeedface... rather than 0xdeadbeef...; the
value it sees is another thread's."

İkinci kontrol (sayaç eşitliği) **yakalamadı.** Sebebi, mutasyonun
sayacı da yalan söyletmesiydi: atlarken de artırıyordu, yani eşitlik
bozulmadı. Bir sayaç, onu güncelleyen kodun doğruluğunu ölçemez;
yalnızca kod ile sayaç arasındaki *tutarsızlığı* ölçer.

Bu, sayaç eşitliğini değersiz kılmıyor ama ne ölçtüğünü daraltıyor:
**kasıtsız** bir atlamayı yakalar (bir `if` yanlış yere konduğunda
sayaç da atlanır), kasıtlı bir sahteciliği yakalamaz. Gerçek lazy
switching uygulaması sayacı dürüstçe tutacağı için eşitlik yine de
doğru soruyu soruyor.

Üçüncü bir gözlem: değer kontrolü mutasyonu **her koşuda değil**,
zamanlamaya bağlı olarak yakaladı. Hangi thread'in hangi anda
preempt edildiğine bağlı. Bu, tek bir mutasyon koşusunun "guard
test ediliyor" kanıtı olmadığını gösteriyor — tekrarlamak gerekti.

Eşitlik tam olmalı, "en az" değil — ama *tam* eşitlik de bir yarış:
sayaçlar üç ayrı atomik okumayla alınıyor ve aralarına bir switch
girebiliyor. Kontrol bu yüzden "bir switch farkına kadar eşit"
diyor. M6'nın ping-pong testi aynı dersi vermişti: kararsız bir test,
gevşek bir testten kötüdür.

## Elenen alternatifler

**Lazy switching (CR0.TS + `#NM`).** Yukarıda: kazanç varsayımsal,
karmaşıklık gerçek, ve CVE-2018-3665'in temeli.

**`xsave`/`xrstor`.** Değişken boyut, TCB'nin sabit boyutuyla
çelişiyor. AVX açılana kadar gerekmiyor, ve AVX OSXSAVE ile kapalı.

**FPU durumunu TCB dışında, ayrı bir Frame'de tutmak.** Her TCB için
bir Frame capability'si daha gerektirirdi — bir thread yaratmanın
maliyetini artırır ve "TCB bir nesnedir" basitliğini bozar. ADR
017'nin 1024 baytlık bağlam alanı zaten bunun için ayrılmıştı.

**Kernel'de de SSE kullanmak.** `-mno-sse` kaldırılabilirdi ve
`memcpy` hızlanırdı. Reddedildi: kernel'in SSE kullanması, her
exception handler'ının da FPU durumu kaydetmesi demek — kesme
yolunun maliyetini 512 bayt artırır, ve kesme yolu syscall'dan bile
sıcak.

## Sonuç

- **Eager switching**: her bağlam değiştirmede `fxsave`/`fxrstor`.
  Lazy reddedildi; belirleyici sebep CVE-2018-3665 sınıfı bir açık
  olması, performans değil.
- Durum TCB'nin **offset 256**'sında, 512 bayt, 16-hizalı. Offset ve
  hizalama derleme zamanında assert ediliyor.
- **`fxsave`, `xsave` değil.** Sabit 512 bayt TCB'ye sığıyor; AVX
  CR4.OSXSAVE temiz tutularak kapatılıyor, yani kaydedilmeyen durum
  erişilemez durum.
- CR0: EM temiz, MP set, TS temiz ve temiz kalıyor. CR4: OSFXSR ve
  OSXMMEXCPT set, OSXSAVE temiz. Hepsi geri okunup doğrulanıyor.
- Yeni thread'in FPU durumu **sıfır değil, temiz**: MXCSR 0x1F80,
  x87 CW 0x037F. Sıfır bir MXCSR tüm istisnaları açar.
- Test iki şey ölçüyor: değerin korunduğu, **ve** `fxsave` sayacının
  switch sayısına eşit olduğu. İkincisi olmadan "lazy yapmadık"
  iddiası test edilmemiş olurdu.
- Maliyeti ölçülüp raporlanıyor, M7'nin 2166/48173 taban çizgisine
  karşı.