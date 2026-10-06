# ADR 009 — Adres uzayı yerleşimi ve user/kernel sınırı

Status: accepted (M3 architect pass)

## Bağlam

M3'te kendi sayfa tablolarımızı kuracağız. Bunu yapmadan önce hangi sanal
adresin neye ait olduğuna karar vermek gerekiyor, çünkü:

- **User/kernel sınırı M8'de değiştirilemez.** Capability modeli, syscall
  ABI'si ve user VSpace'in tamamı bu sınıra göre yazılacak. Sonradan
  kaydırmak her birini etkiler.
- Kernel'in ihtiyaç duyduğu aralıklar (fiziksel pencere, heap, stack'ler,
  MMIO) birbirine girmemeli; her birinin sahibi ve ne zaman dolacağı belli
  olmalı.
- Bazı adresler seçilebilir değil: kernel imajı `-C code-model=kernel`
  yüzünden üst 2 GiB'da olmak zorunda.

x86-64'te 4 seviyeli sayfalama 48-bit sanal adres verir. Kanonik kural
(Intel SDM Vol. 1, 3.3.7.1) bit 63:48'in bit 47 ile aynı olmasını şart
koşar, bu da adres uzayını iki 128 TiB'lık yarıya böler ve aralarında
erişilemez bir boşluk bırakır.

Kanonik olmayan bir adrese erişim **#GP** üretir, #PF değil — kontrol sayfa
yürüyüşünden önce yapılır. Bu ayrım hata ayıklarken önemli ve M3'ün
`translate` API'sinde ayrı bir hata türü olmasının sebebi
([ADR 011](011-sayfa-tablosu-api.md)).

## Seçim

**User alt yarıda, kernel üst yarıda. Sınır `0xFFFF_8000_0000_0000`.**

```
0x0000_0000_0000_0000 .. 0x0000_7FFF_FFFF_FFFF   user   (128 TiB)
                      kanonik boşluk
0xFFFF_8000_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF   kernel (128 TiB)
```

Tam harita [docs/address-space.md](../address-space.md)'de. Kernel yarısının
bölünmesi:

| Taban | Boyut | Sahip |
|---|---|---|
| `0xFFFF_8000_0000_0000` | 64 TiB | PMAP — fiziksel bellek penceresi |
| `0xFFFF_C000_0000_0000` | 8 TiB | Kernel heap (M4) |
| `0xFFFF_C800_0000_0000` | 8 TiB | Kernel stack'leri |
| `0xFFFF_D000_0000_0000` | 8 TiB | MMIO |
| `0xFFFF_D800_0000_0000` | ~40 TiB | Rezerve |
| `0xFFFF_FFFF_8000_0000` | 2 GiB | Kernel imajı |

Dört karar bu yerleşimi belirledi:

**1. Sınır tam ortada.** `0xFFFF_8000_0000_0000` kanonik üst yarının ilk
adresi. Bir adresin hangi yarıda olduğu tek bir karşılaştırma: bit 47 set
mi? Alternatif (kerneli daha küçük bir dilime sıkıştırmak) kontrolü iki
karşılaştırmaya çıkarırdı ve bu kontrol her syscall'da, her user pointer
doğrulamasında çalışacak.

**2. PMAP Limine'in HHDM'i ile aynı adreste.** Limine bu makinede HHDM'i
`0xFFFF_8000_0000_0000`'a koyuyor. Kendi penceremizi aynı yere kurarsak M3
geçişi sırasında daha önce hesaplanmış hiçbir adres geçersizleşmez — frame
allocator'ın bitmap pointer'ı dahil. Ayrıntı [ADR 010](010-hhdm-kaderi.md).

**3. İlk 64 KiB asla eşlenmez.** Null pointer dereference'ı #PF üretmeli.
64 KiB, tek bir sayfadan fazla: küçük bir offset'li null pointer (`(*p).field`
ile p=null) da aynı tuzağa düşsün diye. Windows'un `NULL` koruma bölgesi de
64 KiB; sebep aynı.

**4. User alanının *içi* kernel'i ilgilendirmez.** Kernel yalnızca
`USER_MIN..USER_MAX` sınırını bilir. Bir user bileşeninin stack'ini nereye,
heap'ini nereye koyacağı onun kararıdır ve kernel'e capability invocation
ile bildirilir. Kernel'in "user stack'leri şurada olur" diye bir görüşü
olması, TCB minimum kuralının ihlali olurdu — bu bir politika.

### LA57 (5 seviyeli sayfalama) kullanılmıyor

Limine'den 4 seviyeli sayfalama isteyeceğiz. LA57, 57-bit sanal adres ve
128 PiB'lık yarılar verir; kanonik boşluk kayar ve kernel yarısı
`0xFF00_0000_0000_0000`'da başlar.

Gerekçe: 128 TiB kernel alanı ve 128 TiB user alanı, bu projenin
göreceği her iş yükü için fazlasıyla yeterli. LA57'nin getirisi yok, maliyeti
var: sayfa yürüyüşü bir seviye daha derin, ve harita LA57 ile 4 seviyeli
arasında taşınabilir değil.

**Önemli:** `0xFFFF_8000_0000_0000` LA57 altında da kanoniktir, ama kernel
yarısının *başlangıcı* olmaktan çıkar — ortasında bir yere düşer. Yani bu
harita LA57'ye geçişte **doğrudan taşınmaz**; `KERNEL_MIN` değişir ve tüm
sınır kontrolleri gözden geçirilmelidir. Bunu şimdi yazıyorum ki ileride
"zaten kanonikti, sorun olmaz" diye düşünülmesin.

## Elenen alternatifler

**Kernel'i alt yarıya koymak.** Mimari engel yok, ama `-C code-model=kernel`
üst 2 GiB varsayıyor. Alt yarı için `code-model=large` gerekirdi: her
fonksiyon çağrısı 64-bit mutlak adres üzerinden, gözle görülür bir maliyet.

**Sınırı başka bir yere koymak (örneğin user'a 256 GiB).** Kontrol iki
karşılaştırmaya çıkar ve kazanılan hiçbir şey yok — kernel 128 TiB'ı
doldurmayacak. Üstelik "user adresi mi" sorusu kernel'in en sıcak
yollarından birinde.

**PMAP'i daha küçük tutmak (örneğin 4 TiB).** 64 TiB, 4 seviyeli sayfalamada
adreslenebilir fiziksel belleğin tamamını kapsar ve tablo maliyeti ihmal
edilebilir: 1 GiB sayfalarla eşlenirse 64 TiB için 64 KiB PDPT yeter.
Küçültmek bir gün "ne kadar RAM destekliyoruz" sorusunu doğururdu.

**Fiziksel belleği pencere yerine ihtiyaç duyuldukça eşlemek.** Her çerçeve
erişimi geçici bir eşleme + TLB flush demek. seL4 bunu yapmaz, Linux
yapmaz, ve haklılar: maliyet sürekli, kazanç yalnızca adres alanı — ki o da
bol.

**ASLR (kernel adres uzayını rastgeleleştirmek).** Savunma değeri var ama
şu an yanlış öncelik: hata ayıklamayı zorlaştırır, ve M11'e kadar user kodu
çalıştırmıyoruz. Harita sabit adresler kullanıyor, bu yüzden ileride ASLR
eklemek PMAP/KHEAP/KSTACK tabanlarını çalışma zamanı değişkenlerine
dönüştürmeyi gerektirir — mümkün, ama ayrı bir karar.

## Sonuç

- User/kernel sınırı `0xFFFF_8000_0000_0000`'da **kalıcı olarak sabit**.
  M8'de user space gelince bu değişmeyecek.
- "Bu adres user'a mı ait" sorusu tek bir karşılaştırma.
- PMAP, Limine'in HHDM'i ile aynı adreste başlıyor; M3 geçişi önceden
  hesaplanmış adresleri geçersizleştirmiyor.
- Kernel heap, stack'ler ve MMIO için aralıklar şimdi rezerve edildi; M4 ve
  M6 geldiğinde yer kavgası olmayacak.
- Null guard 64 KiB.
- 4 seviyeli sayfalamaya bağlanıldı. LA57'ye geçiş `KERNEL_MIN`'i
  değiştirir ve ayrı bir ADR gerektirir.
- Harita [docs/address-space.md](../address-space.md)'de ve orada beş
  değişmez listeleniyor; M3 bunları `debug_assert!` ve link sonrası
  denetimle zorlayacak.