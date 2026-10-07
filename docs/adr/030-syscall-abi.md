# ADR 030 — Syscall ABI: tek giriş, hiç yeni çağrı

Status: accepted (M8)

ADR 029 ring 3'e nasıl girilip çıkıldığını karara bağladı. Bu ADR
arada ne olduğunu: user kodu kernel'den bir şey istediğinde hangi
register'da ne taşındığını, kernel stack'ine nasıl geçildiğini, ve
girişin hangi noktalarda bir saldırgana açık olduğunu.

## Tek syscall: `invoke`

ADR 017 sistem çağrısı sayısını **bir**'de sabitledi: her şey bir
capability invocation'ı. M8 buna hiçbir şey eklemiyor.

Bu, master prompt'un 5. kuralının doğrudan uygulanması, ama kuralın
kendisinden bağımsız bir gerekçesi de var: her yeni syscall, kernel'in
saldırı yüzeyine bir giriş noktası ekler ve her giriş noktası kendi
argüman doğrulamasını gerektirir. Tek bir giriş, doğrulamanın tek bir
yerde olması demek — ve o yer test edilebilir.

"Bir syscall" ifadesi bir basitleştirme gibi görünüyor ama değil:
`invoke` bir *dağıtıcı*, ve dağıttığı şeyler M5'in invocation'ları.
Fark şurada: hangi işlemin yapılacağına capability'nin **tipi** ve bir
method numarası karar veriyor, bir syscall numarası değil. Yetki
kontrolü capability'de; syscall numarası sadece bir etiket olurdu.

## Register düzeni

SysV AMD64'ün çağrı düzenine mümkün olduğunca yakın, ama `syscall`'ın
elinden aldığı iki register'ı hesaba katarak.

`syscall` komutu **RCX'i ve R11'i yok eder**: RCX'e dönüş adresini,
R11'e RFLAGS'i yazar. Yani SysV'nin dördüncü tamsayı argümanı (RCX)
kullanılamaz.

| Register | Taşıdığı |
|---|---|
| RDI | capability CPtr |
| RSI | method numarası |
| RDX | argüman 0 |
| R10 | argüman 1 — **RCX değil** |
| R8 | argüman 2 |
| R9 | argüman 3 |
| RAX | (dönüşte) sonuç |
| RCX | `syscall` tarafından ezildi: dönüş RIP |
| R11 | `syscall` tarafından ezildi: RFLAGS |

RCX yerine R10 kullanmak Linux'un da yaptığı şey ve sebebi aynı. Bunu
"Linux böyle yapıyor" diye değil, alternatifi olmadığı için
yazıyorum: RCX'i kullanmaya çalışan bir ABI, user kodunun `syscall`
komutundan *önce* RCX'i bir yere kopyalamasını gerektirirdi, ve o
kopyalama yine bir register tüketirdi.

Dönüş tek bir `u64` (RAX). Hata kodları negatif değil, ayrı bir
alanda da değil — `CapError`'ın ayrık bir temsili. İki değer döndürmek
(sonuç + hata) M9'un IPC'sinde gerekecek ve orada karara bağlanacak;
şimdi gerekmiyor ve gerekmeyen bir şeyi tasarlamak, yanlış tasarlamanın
en kolay yolu.

## FMASK: hangi bayraklar maskeleniyor

`IA32_FMASK`, `syscall` sırasında RFLAGS'ten **temizlenecek** bitleri
tutar. İki bit zorunlu, üçüncüsü tartışmalı:

**IF (bit 9) — zorunlu.** `syscall` bir kesme kapısı değil; CPU IF'i
kendiliğinden temizlemez. Maskelenmezse, kernel'e giriş ile kernel
stack'ine geçiş arasındaki birkaç komutluk pencerede bir kesme
gelebilir. O anda RSP hâlâ **user'ın** — yani kesme handler'ı,
user'ın kontrol ettiği bir adrese çerçeve iter. Doğrudan bir açık.

**DF (bit 10) — zorunlu.** SysV ABI, bir C fonksiyonuna girişte DF'in
temiz olmasını şart koşuyor. User kodu DF'i set bırakırsa, kernel'in
ilk `rep movs`'u belleği ters yönde kopyalar. M1'in exception stub'ı
bunu `cld` ile çözüyordu; burada FMASK aynı işi komut harcamadan
yapıyor.

**AC (bit 18) — ve bu, M7'nin bir dersi.** SMAP, RFLAGS.AC set ise
kernel'in user sayfalarına erişmesine izin verir. User kodu AC'yi
kendi set edebilir (`stac` ayrıcalıklı değil — sadece CR4.SMAP
gerektirir ve CPL kontrolü yoktur). AC maskelenmezse, user kodu AC'yi
set edip `syscall` yapar ve kernel SMAP koruması olmadan çalışmaya
başlar.

Bu, M7'de `UserAccess` guard'ını yazarken fark edilmedi: o guard
kernel'in *kendi* erişimlerini yönetiyor ve "AC zaten set ise
dokunma" diyor. User'dan miras alınan bir AC'yi tam olarak "zaten set"
sayar ve guard hiçbir şey yapmaz — hem `stac`'i hem `clac`'i atlar.
Koruma sessizce yok olur.

FMASK'e AC eklemek bunu kökten çözüyor: kernel'e her giriş AC temiz
başlıyor, kaynağı ne olursa olsun.

Maskelenen set: `IF | DF | AC`. Trap bayrağı (TF, bit 8) de maskeleniyor
— user kodu TF set edip `syscall` yaparsa kernel'in ilk komutunda bir
`#DB` alınır, ve tek adım hata ayıklama kernel'in içinde dolaşmaya
başlar.

### Mutasyon testi: kanıtın kaynağı guard'ın yazarı olamaz

AC bitini `FMASK_BITS`'ten silip testi koştum. **Yakalandı — ama
yanlış sebeple.**

Kırılan tek şey şu satırdı:

```
[cpu] syscall: fmask clears IF|DF|TF|AC
```

Yani testin elindeki tek kanıt, kernel'in kendi yazdırdığı bir
cümleydi. Biti silen commit o cümleyi de düzeltseydi — ki düzeltmek
doğal olurdu, mesaj artık yanlış olacaktı — mutasyon görünmez
kalırdı. Koruma, kendi yokluğundan ayırt edilemiyordu.

Bu, M5'in `SlotNotEmpty` ve M7'nin K1 bulgularıyla aynı şekil, ama
yeni bir çeşidi: orada iki guard aynı hatayı veriyordu, burada
guard'ın kanıtı guard'ın yazarının kontrolündeydi.

**Düzeltme:** `cpu::fmask()` MSR'ı geri okuyor ve test dört biti tek
tek doğruluyor. İkinci denemede mutasyon davranıştan yakalandı:

```
[PANIC] IA32_FMASK does not clear AC on syscall entry: ... The MSR
        holds 0x0000000000000700.
```

Artık banner'ı değiştirmek kurtarmıyor. ADR 023'ün K5'i bir adım
daha keskinleşiyor: **bir kontrolün kanıtı, o kontrolü kaldıran
değişikliğin de değiştirebileceği bir şey olamaz.**

## Kernel stack'ine geçiş: `swapgs` ve neden kullanılmıyor

`syscall` RSP'ye dokunmaz. Kernel'e girildiğinde RSP hâlâ user'ın
yığınını gösteriyor ve ilk iş onu değiştirmek — ama değiştirmek için
kernel stack'inin adresini bir yerden okumak gerekiyor, ve okumak için
bir register gerekiyor, ve hangi register'ı ezersek user'ın değerini
kaybediyoruz.

Standart çözüm `swapgs`: GS tabanını bir CPU-başına veri yapısına
çevirir ve `mov rsp, gs:[offset]` ile stack adresi okunur. Hiçbir
genel register harcanmaz.

**Kullanılmıyor.** Sebep: `swapgs`'in kendi tuzağı var ve bu kernel'de
onu çözmenin bedeli, çözdüğü sorunun bedelinden yüksek.

`swapgs` bir *takas*, bir yükleme değil. Kernel'e girişte bir kez,
çıkışta bir kez çalışmalı. Bir kesme, kernel'e girişten sonra ama
`swapgs`'ten önce gelirse — ya da ring 3'ten gelen bir exception'da
handler `swapgs` yaparken zaten kernel bağlamındaysa — GS yanlış
yöne döner ve kernel, user'ın kontrol ettiği bir tabana göre adresleme
yapar. Doğru çözüm, her exception handler'ında "nereden geldim"
kontrolü (CS'in RPL'ine bakmak) ve koşullu `swapgs`. Bu, M1'in 34
stub'ının her birine bir dal ekler.

Yerine kullanılan şey: **tek CPU, tek değişken.**

```
  mov gs:[0], rsp     -- hayır, GS yok
```

Gerçek yol daha basit. Kernel stack'inin adresi bir `static`te
duruyor ve `syscall` girişinde oradan okunuyor:

```
  syscall_entry:
      mov [rip + SAVED_USER_RSP], rsp
      mov rsp, [rip + KERNEL_RSP]
      ...
```

RIP-göreli adresleme hiçbir genel register harcamıyor. İki `static`
de kernel yarısında, yani her adres uzayından erişilebilir (ADR 027).

Bunun **tek CPU'ya bağımlı** olduğu açık ve kabul ediliyor: iki CPU
aynı `SAVED_USER_RSP`'ye yazarsa biri diğerinin değerini kaybeder.
SMP geldiğinde bu `swapgs` ve CPU-başına veri olacak; bilinen
eksiklere yazıldı. Şimdi yazmamanın gerekçesi, yanlış soyutlamayı
erken dondurmamak: CPU-başına veri yapısının şekli, SMP'nin başka
gereksinimleriyle birlikte tasarlanmalı.

`KERNEL_RSP` her thread switch'inde güncelleniyor, `TSS.RSP0` ile
birlikte ve aynı yerde. İkisi aynı değer ama iki farklı yoldan
okunuyor: RSP0'ı CPU okuyor (kesmelerde), `KERNEL_RSP`'yi syscall
stub'ı. Tek bir değişken yapılabilirdi — TSS'in içindeki alanı
okumak — ama TSS'in düzeni donanım tarafından dayatılıyor ve ona
RIP-göreli erişmek için TSS'in adresini bilmek gerekiyor, bu da bir
dolaylama daha demek. İki değişkenin tutarlılığı bir `debug_assert`
ile bağlanıyor.

## Giriş yolunun sırası

```
syscall_entry:
    1. user RSP'yi sakla, kernel RSP'yi yükle
    2. ExceptionFrame'e benzer bir çerçeve kur
       (RCX = dönüş RIP, R11 = RFLAGS -- bunlar çerçevenin parçası)
    3. 15 genel register'ı it
    4. Rust handler'ı çağır (RDI = çerçeve işaretçisi)
    5. register'ları geri al
    6. RCX kanonik mi? değilse iretq (ADR 029)
    7. user RSP'yi geri yükle
    8. sysretq
```

Adım 3'te **on beş** register'ın hepsi kaydediliyor, SysV'nin
caller-saved ayrımına bakılmaksızın. M1'in exception stub'ıyla aynı
gerekçe ama farklı bir sebeple: exception'da sebep "kod herhangi bir
komut sınırında kesildi"; burada sebep **user'ın register'ları
kernel'e sızdırılmamalı ve kernel'in register'ları user'a
sızdırılmamalı**.

İkinci yön daha az belirgin: dönüşte geri yüklenmeyen bir register,
kernel'in içinde ne varsa onu user'a verir. Bir çekirdek adresi, bir
capability'nin fiziksel adresi, bir kullanıcı verisinin kalıntısı.
Hepsini kaydedip hepsini geri yüklemek bu sızıntı sınıfını bütünüyle
kapatıyor.

Çerçevenin düzeni `ExceptionFrame`'e kasten benzetiliyor ama **aynı
değil** ve aynı olmaya zorlanmadı. Syscall'da CPU hiçbir şey itmiyor;
`ExceptionFrame`'in son beş alanı (RIP, CS, RFLAGS, RSP, SS) burada
stub tarafından, farklı kaynaklardan dolduruluyor. İkisini tek bir
tipte birleştirmek, "bu alan nereden geldi" sorusunu her kullanımda
belirsiz yapardı.

## Argüman doğrulama: kernel user'a hiç güvenmiyor

Handler'ın aldığı her şey user'dan geliyor ve hiçbiri güvenilir değil.

**CPtr.** M5'in `cptr::resolve`'ı zaten her alanı doğruluyor:
reserved bitler, iki indeks aralığı, iki generation, ve root slot'un
CNode olması (ADR 018). Uydurma bir CPtr `InvalidCPtr`,
`CPtrOutOfRange` veya `StaleCPtr` döndürüyor — hiçbiri panic değil.
`user-cap-forge` senaryosu bunu ring 3'ten sınıyor.

**Method numarası.** Bir `match`, ve eşleşmeyen her şey
`InvalidArgument`. Bir tablo araması değil: tablo, indeksin sınır
kontrolünü ayrı bir adım yapardı ve o adımın unutulması klasik bir
açık.

**İşaretçi argümanları yok.** M8'in invocation'larının hiçbiri user
belleğine işaretçi almıyor. Bu bir tasarım kısıtı değil, şimdilik bir
olgu — M9'un IPC'si alacak, ve orada her işaretçi için "user yarısında
mı, eşli mi, haklar yeterli mi" üçlüsü gerekecek. Şimdi olmaması,
M8'in saldırı yüzeyini ölçülebilir derecede küçültüyor.

**Kernel çökmemeli.** Her hata bir `Err` dönüşü. ADR 023'ün K5'i
burada en sert haliyle geçerli: her `Err` yolu tetiklenmiş olmalı,
yoksa o yol test edilmemiş demektir — ve test edilmemiş bir hata yolu,
düşman girdi altında ilk kırılan şeydir.

## Ölçüm

`user-test` iki yolu da zamanlıyor: `sysret` ile dönen syscall'lar ve
`iretq` ile dönenler. ADR 029 `iretq`'in "~100 çevrim daha pahalı"
olduğunu tahmin ediyor; o tahmin ya doğrulanacak ya düzeltilecek.

Emülasyon şerhi M6 ve M7'dekiyle aynı: QEMU'nun TCG yorumlayıcısında
komut maliyetleri gerçek silikonu yansıtmıyor, ve `sysret`/`iretq`
farkı özellikle mikro-mimari (pipeline boşaltma, segment descriptor
okuma) — yani emülasyonun en kötü modellediği şey. Çıkan sayı bir
*oran* olarak bile şüpheli; mutlak değer olarak hiç anlamlı değil.

## Elenen alternatifler

**Birden çok syscall.** ADR 017 reddetti, M8 açmıyor.

**`int 0x80` tarzı bir yazılım kesmesi.** Çalışır ve çok daha yavaş
(~300 çevrim vs ~100). Tek avantajı, IDT gate'inin IF'i otomatik
temizlemesi — ki FMASK bunu zaten yapıyor.

**`swapgs` ve CPU-başına veri.** Yukarıda. SMP'de gerekli, şimdi
değil.

**RCX'i argüman olarak kullanmak.** `syscall` onu eziyor; mümkün
değil.

**Hata kodunu ayrı bir register'da döndürmek.** RDX boşta ve
kullanılabilirdi. Reddedildi çünkü M9 gerçek bir çok-değerli dönüş
isteyecek (IPC mesajı + gönderen kimliği + hata), ve iki değerlik bir
geçici çözüm o tasarımı önceden kısıtlardı.

**AC'yi FMASK'e koymamak.** Üstte anlatılan sessiz SMAP kaybı.
Fark edilmesi, M7'nin `UserAccess` guard'ının "zaten set ise dokunma"
kuralını user'dan gelen bir AC ile birlikte düşünmeyi gerektirdi.

## Sonuç

- **Tek syscall**, `invoke`. Hangi işlem olduğuna capability tipi ve
  method numarası karar veriyor; syscall numarası diye bir şey yok.
- Register düzeni SysV'ye yakın ama **RCX yerine R10**: `syscall`
  RCX ve R11'i yok ediyor.
- **FMASK = IF | DF | AC | TF.** AC'nin orada olması M7'nin bir
  dersi: user kodu `stac` çalıştırıp syscall yapabilir, ve
  `UserAccess` guard'ı "zaten set" görüp hiçbir şey yapmaz — SMAP
  sessizce kaybolur. **Mutasyonla bulundu:** ilk halinde bu guard'ın
  tek kanıtı kernel'in yazdırdığı bir cümleydi; artık MSR geri
  okunuyor.
- Kernel stack'ine geçiş **`swapgs` ile değil**, RIP-göreli iki
  `static` ile. Tek CPU'ya bağımlı ve bu kabul edilmiş bir borç;
  SMP'de `swapgs` ve CPU-başına veri gerekecek.
- `KERNEL_RSP` ve `TSS.RSP0` her thread switch'inde birlikte
  yazılıyor; tutarlılıkları assert ediliyor.
- **On beş register da kaydediliyor**, iki yönlü sızıntıyı kapatmak
  için: user'ınki kernel'e, kernel'inki user'a.
- Her user argümanı doğrulanıyor, her hata `Err` dönüyor, kernel
  çökmüyor. M8'in invocation'ları **user işaretçisi almıyor** — M9
  alacak, ve o doğrulama orada tasarlanacak.