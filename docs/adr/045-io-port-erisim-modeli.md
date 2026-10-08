# ADR 045 — I/O portlarına erişim: IOPort capability'si

Status: accepted (M13)

## Bağlam

M13 seri portu kernel'den çıkarıyor. Sürücü ring 3'te bir bileşen
olacak ve 0x3F8–0x3FF aralığındaki sekiz UART yazmacına erişmesi
gerekiyor.

Bu, projenin şimdiye kadar çözmediği bir sorun. ADR 017'nin device
Untyped modeli **MMIO için** tasarlanmıştı: cihazın fiziksel adres
aralığı bir Untyped olur, ondan Frame türetilir, Frame bileşenin adres
uzayına eşlenir, bileşen normal bellek erişimiyle cihazı sürer. Kernel
yalnızca eşlemeyi kurar ve sonra aradan çıkar.

x86'nın I/O portları bu modele **girmiyor**, çünkü bir port adres
uzayında değil. `in`/`out` ayrı bir 16-bitlik adres uzayına erişen ayrı
komutlar ve bunlara izin verilip verilmediğini sayfa tablosu değil,
CPL ile IOPL'in karşılaştırması ve TSS'teki izin haritası belirliyor
(Intel SDM Vol. 1, 19.3 "I/O Instructions"; Vol. 3A, 20.5 "I/O
Permission Bit Map").

Yani: eşleyebileceğimiz bir sayfa yok. Üç seçenek var.

## Seçenekler

### (a) Sürücü thread'inin IOPL'ini 3 yapmak

`RFLAGS.IOPL` (bit 13:12) CPL'den büyük veya eşitse, o thread **tüm
65536 porta** sınırsız erişir (SDM Vol. 3A, 20.4 "I/O Privilege
Level"). TCB'nin kayıtlı RFLAGS'ine 0x3000 yazmak yeterli.

Üç satırlık değişiklik. Ve yanlış.

Bu modelde seri sürücü, PCI yapılandırma uzayını (0xCF8/0xCFC) okuyup
yazabilir, CMOS/RTC'yi (0x70/0x71) değiştirebilir, eski DMA
denetleyicisini programlayabilir, klavye denetleyicisi üzerinden
makineyi yeniden başlatabilir (0x64'e 0xFE). Bir UART sürücüsünün
yetkisi, makinenin tamamının yetkisi olur.

**Kabul kriteri 5 bu seçenekle sağlanamaz.** Senaryo, sürücünün
0x70'e erişmeye çalışıp engellenmesini istiyor. IOPL=3 ile engellenmez
— başarılı olur. Yani (a), testin ölçmek istediği şeyi var olmayan bir
şey haline getiriyor: "sınırlı erişim" iddiası, kendi yokluğundan
ayırt edilemez (ADR 023, K5).

Ayrıca IOPL thread başına RFLAGS'te taşınıyor, yani iptal edilebilir
bir şey değil. Sürücü ölünce yetkiyi geri almak, TCB'yi yok etmekten
geçiyor — ki bu işe yarıyor, ama yalnızca tesadüfen: yetki nesneye
değil thread'e bağlı olduğu için, yetkiyi *devretmek* imkânsız ve
*daraltmak* imkânsız.

### (b) TSS I/O izin haritası

CPL > IOPL iken CPU, `in`/`out` için TSS'teki bitmap'e bakar: port
numarasına karşılık gelen bit 0 ise erişime izin verilir, 1 ise #GP
(SDM Vol. 3A, 20.5.2). 65536 port için 8 KiB.

Bu gerçek, donanımla uygulanan, port başına kesinlikte bir izin. (a)'ya
göre ciddi bir gelişme ve kriter 5'i sağlar: 0x3F8–0x3FF açık, 0x70
kapalı, ihlal #GP ile sonuçlanır.

Ama üç sorunu var ve üçüncüsü belirleyici.

**Birincisi, bitmap TSS'in — thread'in değil.** Bu kernel'de bir TSS
var (tek CPU, `gdt.rs`). Bitmap'i thread başına yapmak için her
bağlam değişiminde 8 KiB'ı yeniden yazmak ya da TSS'i takas etmek
gerekir. İlki anlamsız pahalı; ikincisi GDT'de thread başına
descriptor demek. Yazmazsak, bitmap "o an hangi thread koşuyorsa onun"
değil, **sistemin** izin haritası olur: seri sürücüye verilen izin,
tüm ring 3 thread'leri için açık kalır. Kriter (c) — sürücü olmayan
bir bileşenin 0x3F8'e yazamaması — o zaman sağlanamaz.

**İkincisi, iptal mekanizması yok.** Sürücü ölünce bitmap'teki bitleri
kim temizler? Capability sistemi iptali zaten biliyor (ADR 019'un
türetme ağacı), ama yalnızca bir *nesneyi* adlandıran capability'ler
için. Bir bitmap biti nesne değil; `revoke` onu bulamaz. Her yok etme
yoluna "bu thread'in portlarını da temizle" eklemek, unutulabilecek bir
muhasebe borcu yaratır — ADR 041'in `IRQHandler`'ı nesne yapma
gerekçesi tam olarak buydu.

**Üçüncüsü ve asıl olanı: bitmap biti bir capability değil.** Kopyalanamaz,
badge alamaz, daraltılarak devredilemez, türetme ağacında görünmez.
Sistemde "capability olmayan yetki" olarak yalnızca bir şey var
(`IRQControl`) ve o bile bir capability — nesnesi olmayan ama
slot'ta duran, kopyalanabilen, iptal edilebilen bir capability. Bir
bitmap biti bunların hiçbiri değil. Onu eklemek, "her erişim bir
capability üzerinden" kuralına ikinci bir istisna açmak olur ve bu
istisna birincisinden farklı olarak *adlandırılamaz*.

### (c) IOPort capability'si — seçilen

Yeni bir nesne tipi. Bir `IOPort`, bir port **aralığına** yetki veriyor
ve üzerinde iki invocation var:

```
IOPort.Out8(port, value)   -> 0
IOPort.In8(port)           -> byte
```

Kernel, `port`'un capability'nin aralığında olup olmadığını denetliyor
ve `in`/`out` komutunu **kendisi** yürütüyor. Sürücü thread'inin IOPL'i
0 kalıyor; doğrudan `out` denemesi #GP alıyor.

## Karar

(c). `IOPortControl` ve `IOPort` olmak üzere **iki** yeni tip.

### Neden iki tip, bir değil

Brief "onuncu tip" diyordu; bu onuncu ve on birinci. Gerekçe
ADR 041'in gerekçesinin aynısı, çünkü sorun aynı:

Tek tip olsaydı, `IOPort` ya Untyped'dan `retype` ile üretilirdi ya da
bir `IOPort` sahibi yeni `IOPort`'lar üretebilirdi.

- **Retype ile üretilebilseydi**, belleğe sahip olmak porta sahip
  olmak anlamına gelirdi. Her bileşenin bütçesi var (ADR 022); o
  zaman her bileşen istediği portu alabilirdi.
- **IOPort'tan IOPort üretilebilseydi**, 0x3F8 sahibi 0x70 için bir
  tane üretebilirdi — yetki yükseltme.

Yani yetkiyi *dağıtma* hakkı ile yetkiyi *kullanma* hakkı ayrı
olmalı. `IRQControl`/`IRQHandler` ikilisi bu ayrımın zaten var olan
hali; `IOPortControl`/`IOPort` onun kopyası değil, aynı kuralın ikinci
uygulaması.

### Aralık, tek port değil

Bir `IOPort` `[first, last]` kapalı aralığını adlandırıyor. UART'ın
sekiz yazmacı (0x3F8..0x3FF) tek bir capability. Port başına
capability, sürücünün sekiz slot taşıması ve her erişimde doğru olanı
seçmesi demekti — ve bir yazmaç ofsetini yanlış capability'ye
uygulamak, aralık denetiminin yakalayamayacağı bir hata olurdu.

### Tek sahiplik

`IOPortControl.Issue` bir aralığı verirken, o aralığın **hiçbir
parçasının** başka bir `IOPort` tarafından tutulmadığını denetliyor.
Gerekçe ADR 041'in tek-sahip kuralıyla aynı: iki bileşenin aynı UART'ı
sürmesi, hangisinin DLAB'ı set ettiği sorusuna cevabı olmayan bir
durum. Bir aygıtı paylaşmak, aygıtı *sahiplenen* bir çoklayıcı
bileşenin işi.

Bu aynı zamanda kriter (c)'yi anlamlı kılıyor: ikinci bir bileşen
0x3F8 istediğinde `IoPortsAlreadyOwned` alıyor, "erişemedi" değil
"**sahibi var**" diyor.

### Genişlik: yalnızca 8 bit

seL4'te In8/In16/In32/Out8/Out16/Out32 var. Burada yalnızca 8 bit,
çünkü 16550A bayt genişliğinde ve 16/32 bitlik erişimi test edecek bir
aygıt yok. Test edilemeyen kod yazmamak, ABI'yi dar tutmaktan daha
önemli — ve ABI kuralı ekleme yönünde serbest (metot numaraları
*eklenir*, araya sokulmaz), yani 16 bit gerektiğinde eklenebilir.

## seL4 ile ilişki — ne alındı, ne alınmadı

seL4'te `seL4_X86_IOPort` nesnesi ve `seL4_X86_IOPortControl` var;
`IOPortControl_Issue` bir aralık için IOPort üretiyor. **Bu tasarım
oradan alındı** ve bunu saklamanın anlamı yok: problem aynı, x86 aynı,
ve seL4'ün cevabı doğru.

Alınmayan iki şey var.

**Genişlik çeşitliliği alınmadı** — yukarıdaki gerekçeyle.

**seL4'ün "ilk IOPortControl tüm aralığa sahiptir, Issue böler"
modelindeki bölünme semantiği alınmadı.** seL4'te Issue edilen aralık
IOPortControl'dan *düşülmez*; denetim, çakışan bir Issue'nun
reddedilmesiyle yapılır. Burada da öyle — ama bunu seL4 öyle yaptığı
için değil, ADR 020'nin Untyped'ı bir *watermark* yapan kararıyla
tutarlı olduğu için: tüketilen kaynak geri verilmiyor, ve kim neyi
aldıysa türetme ağacında duruyor.

Üçüncü bir fark daha var ve bu bir eksiklik: seL4'te IOPort
capability'leri de diğerleri gibi CSpace'te taşınıyor ve
`seL4_CNode_Revoke` ile iptal ediliyor. Burada da öyle — ama buradaki
`IOPort` bir Untyped'dan yer alıyor (32 bayt), yani onu iptal etmenin
iki yolu var: capability'yi silmek, ya da altında durduğu Untyped'ı
iptal etmek. İkincisi `driver-dies` senaryosunun kullandığı yol.

## Elenen bir dördüncü seçenek

**Portları MMIO gibi gösteren bir soyutlama.** "Sanal bir Frame, ona
yazmak porta yazmak demek" — sayfa hatası tuzağıyla. Bu, her bayt için
bir #PF, bir komut çözümleme (kernel'de bir x86 disassembler!) ve bir
emülasyon demek. Hem syscall'dan yavaş, hem de kernel'e asla
istemeyeceğimiz bir bileşen ekliyor. Reddedildi.

## Maliyet — dürüst hesap

Her bayt bir syscall. 115200 baud'da bir karakter ~87 µs sürüyor;
syscall'ın round-trip'i ölçülen değerlerle ~1 µs mertebesinde. Yani
seri port için syscall maliyeti **cihazın kendisinden iki kat
büyüklük mertebesi küçük** ve görünmez.

Bu bir tesadüf değil, bir sınıf özelliği: I/O portları mimarinin eski
ve yavaş yolu. Hız gerektiren her modern aygıt MMIO kullanıyor ve MMIO
zaten ADR 017'nin device Untyped modeliyle — kernel aradan tamamen
çıkarak — çözülmüş durumda. Yani bu ADR'nin maliyeti, maliyetin
önemsiz olduğu yere düşüyor.

Gigabit NIC'i port I/O ile sürmek isteyen biri için bu tasarım yanlış
olurdu. Öyle bir NIC yok.

## Sonuç

- `IOPortControl` (tip 10): aralık dağıtma yetkisi. Boot'ta bir tane
  üretilir, root task'a verilir. Retype ile üretilemez.
- `IOPort` (tip 11): `[first, last]` aralığına erişim. 32 bayt,
  `IOPortControl.Issue` ile ve yalnızca onunla üretilir.
- `IOPort.In8` / `IOPort.Out8`: aralık denetimi kernel'de, `in`/`out`
  kernel'de, sürücünün IOPL'i 0.
- Çakışan aralıklar reddedilir (tek sahiplik, ADR 041 ile aynı
  gerekçe).
- 16/32 bitlik erişim yok; gerekirse eklenir.