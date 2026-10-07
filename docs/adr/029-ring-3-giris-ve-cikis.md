# ADR 029 — Ring 3'e giriş ve çıkış: `iretq` ile gir, `sysret` ile dön

Status: accepted (M8)

M7 bir thread'e kendi adres uzayını verdi ve orada durdu: kod hâlâ
CPL 0'da çalışıyordu. M8 eksik olan tek şeyi ekliyor — ayrıcalık
seviyesi. Bu ADR, ring 3'e *nasıl* girildiğini ve ondan *nasıl*
dönüldüğünü karara bağlıyor; syscall'ın ne yaptığı ADR 030'da.

İki yön simetrik değil ve bu kasıtlı.

## Giriş: `iretq`, `sysret` değil

Ring 3'e geçmenin iki yolu var ve ikisi de "dönüş" komutları — x86'da
ayrıcalık seviyesini *düşürmenin* başka yolu yok. CPU'yu daha az
ayrıcalıklı bir seviyeye koymak, oraya geri dönüyormuş gibi yapmayı
gerektiriyor.

**`iretq`** yığından beş değer alır: RIP, CS, RFLAGS, RSP, SS. CS'in
alt iki biti (RPL) 3 ise CPU ayrıcalık seviyesini düşürür ve SS:RSP'yi
de yükler.

**`sysretq`** RCX'i RIP, R11'i RFLAGS yapar, CS ve SS'i STAR'dan
türetir. Daha hızlı, ama RSP'ye **dokunmaz** — user stack'ini çağıran
kurmak zorundadır.

Bir thread'i *ilk kez* ring 3'te başlatmak için `iretq` seçildi:

1. **Beş alanın beşi de açıkça yazılıyor.** `sysret` RFLAGS'i R11'den
   alır ve CS/SS'i MSR'dan türetir; yeni bir thread için bu üç değerin
   de doğru olduğunu ayrıca kurmak gerekir. `iretq` ile hepsi tek bir
   yığın çerçevesinde görünür — ve yanlış olan varsa yığında durur,
   bir register'da değil.
2. **Hata ayıklanabilir.** Bir `#GP` geldiğinde yığın çerçevesi hâlâ
   orada; hangi alanın bozuk olduğu okunabilir. `sysret`'in `#GP`'si
   ise RCX ve STAR arasında bir yerde.
3. **Zaten var.** M1'in exception dispatcher'ı `iretq` ile dönüyor ve
   çerçeve düzeni (`ExceptionFrame`'in son beş alanı) M1'den beri
   kullanımda. Yeni bir mekanizma değil, var olanın bir user
   seçiciyle beslenmesi.

İlk girişin maliyeti bir kez ödeniyor. Sonraki her dönüş `sysret` ile,
çünkü orada hız syscall başına ödeniyor.

## Dönüş: `sysret`, ve neden tehlikeli

Bir syscall'dan dönüş `sysretq` ile. Ama `sysret`'in bir tuzağı var ve
bu tuzağın bir CVE numarası var.

### Kanonik olmayan RCX (CVE-2012-0217 sınıfı)

`sysretq`, RCX'i RIP'e yükler. RCX kanonik değilse — yani bit 47..63
sign-extension kuralını bozuyorsa — Intel CPU'larda `#GP` **ring 3'e
geçildikten sonra değil, geçilmeden önce, ama kernel stack'i ile**
alınır. Yani:

- Hata, `sysret` tamamlanmadan önce CPL 0'da raporlanır.
- Ama SS zaten user değerini almış olabilir ve RSP hâlâ user'ın.
- Sonuç: kernel kodu, user'ın kontrol ettiği bir stack üzerinde
  `#GP` handler'ını çalıştırır. Bu bir ayrıcalık yükseltme açığı.

AMD ve Intel bu durumu farklı ele alıyor, ve bu farkın kendisi
tehlikenin bir parçası: bir platformda çalışan kod diğerinde açık
veriyor. 2012'de Xen, FreeBSD, NetBSD, Windows ve birkaç başka
çekirdek aynı hatayı aynı anda yaşadı.

**Karar: `sysret` öncesi RCX kanoniklik kontrolü, değilse `iretq`'e
düş.**

```
  ; rcx = dönülecek RIP
  mov r10, rcx
  shl r10, 16
  sar r10, 16          ; bit 47'yi yukarı doğru yay
  cmp r10, rcx
  jne .use_iretq       ; kanonik değil
  sysretq
```

Üç komut. `shl`/`sar` çifti bit 47'yi 63'e kadar yayıyor; sonuç
orijinalle aynıysa adres kanonikti.

`iretq`'e düşmek neden güvenli: `iretq` ayrıcalık geçişini *atomik*
yapar ve kanonik olmayan bir RIP'i ring 3'te, user stack'inde,
`#GP` olarak raporlar — yani hatayı user'ın kendi bağlamında. Yavaş
ama doğru, ve bu yola yalnızca user kodu zaten bozuk bir adrese
dönmeye çalışırken giriliyor.

**Bunu test etmek zorunlu.** Bir guard'ın kendi yokluğundan ayırt
edilemez olması bu projede iki kez yaşandı (ADR 023 K5). `user-test`
içinde bir thread kasten kanonik olmayan bir RIP'e dönüyor; beklenen
şey kernel'in çökmemesi ve `#GP`'nin user bağlamında raporlanması.

### `sysret` ve SS: sessiz bir fark

`sysretq` SS'i `STAR[63:48] + 8` seçicisine yükler, **ama descriptor'ı
GDT'den okumaz** — sadece seçiciyi ve sabit bir descriptor'ı yazar.
Yani o slot'ta ne olduğu `sysret` için önemsiz. Önemli olan `iretq`
için: o GDT'ye bakar.

Sonuç: ikisinin de çalışması için GDT'de **gerçekten** bir user data
segment'i olmak zorunda, ve STAR'ın gösterdiği yerde olmak zorunda.
İkisi tutarsızsa `sysret` çalışır, `iretq` `#GP` verir — ve test
yalnızca birini koşuyorsa fark görünmez. Bu yüzden `user-test` her
ikisini de kullanıyor: ilk giriş `iretq`, her syscall dönüşü `sysret`.

## GDT düzeni: sıra `sysret`'in dayattığı sıra

`STAR[63:48]`, `sysret`'in kullanacağı seçici tabanı. CPU şunu yapar:

| `sysret` türü | CS | SS |
|---|---|---|
| `sysretq` (64-bit) | `STAR[63:48] + 16`, RPL 3 | `STAR[63:48] + 8`, RPL 3 |
| `sysretl` (compat) | `STAR[63:48] + 0`, RPL 3 | `STAR[63:48] + 8`, RPL 3 |

Yani **user CS32, user SS, user CS64 bu sırayla** olmak zorunda. 64-bit
kod segmenti 32-bit'ten *sonra* geliyor, ki ilk bakışta ters görünür.

`STAR[47:32]` ise `syscall`'ın kullanacağı taban:

| | CS | SS |
|---|---|---|
| `syscall` | `STAR[47:32] + 0`, RPL 0 | `STAR[47:32] + 8`, RPL 0 |

Yani **kernel CS, hemen ardından kernel SS**.

M1'in GDT'si zaten doğru sırayla başlıyor (kernel code 1, kernel data
2) ve TSS 3-4'ü kaplıyor. User segmentleri 5'ten başlıyor:

```text
  0  0x00  null
  1  0x08  kernel code, ring 0, 64-bit     <- STAR[47:32] buraya
  2  0x10  kernel data, ring 0
  3  0x18  TSS (16 bayt, 3 ve 4)
  4  0x20    (TSS'in üst yarısı)
  5  0x28  user code 32-bit (yer tutucu)   <- STAR[63:48] buraya
  6  0x30  user data, ring 3
  7  0x38  user code 64-bit, ring 3
```

Slot 5 **kullanılmıyor** ama boş da bırakılamıyor: `sysretl` bu
kernel'de hiç çalıştırılmıyor, ama slot'un varlığı 6 ve 7'nin doğru
offsetlerde olmasını sağlıyor. Geçerli bir 32-bit user kod
descriptor'ı yazılıyor — yer tutucu olarak `0` bırakmak, bir gün
`sysretl` çalışırsa sessiz bir `#GP` demek olurdu.

Seçiciler RPL 3 ile kullanılıyor: `0x33` (user CS64 = `7 << 3 | 3`) ve
`0x2b` (user SS = `6 << 3 | 3`).

**Yanlış sıra teşhis edilmesi zor bir hata.** `sysret` sessizce yanlış
bir descriptor yükler ve hata ilk user komutunda, tamamen ilgisiz bir
yerde görünür. Bu yüzden bir `const` assertion GDT indekslerini STAR'ın
beklediği aritmetiğe bağlıyor: slot numaraları değişirse build kırılır,
çalışma zamanında bir `#GP` olarak değil.

## TSS.RSP0: her switch'te

Ring 3'ten kernel'e giren her şey — kesme, exception, syscall — bir
kernel stack'ine ihtiyaç duyar. Kesme ve exception için CPU bunu
`TSS.RSP0`'dan alır. (Syscall almaz; ADR 030'a bakın.)

RSP0 **thread başına** olmak zorunda, çünkü her thread'in kendi kernel
stack'i var (M6'dan beri). İki user thread'i aynı RSP0'ı paylaşsaydı:
thread A kernel'e girer, kernel stack'ine yazar, preempt edilir;
thread B kernel'e girer, **aynı** stack'in tepesinden yazmaya başlar
ve A'nın çerçevesini ezer. A geri döndüğünde yığını bozuk.

Belirtisi rastgele bozulma — stack'in neresinin ezildiğine bağlı olarak
yanlış bir dönüş adresi, bozuk bir lokal değişken, ya da hiçbir şey.
Deterministik değil ve kaynağı, bozulmanın görüldüğü yerden çok uzakta.

Bu yüzden `switch_to` CR3'ü yüklediği yerde RSP0'ı da yazıyor. Aynı
fonksiyon, aynı gerekçe: thread değiştiyse CPU'nun o thread hakkında
bildiği her şey değişmeli.

Test bunu doğrudan ölçüyor: her syscall hangi stack'e girildiğini
raporluyor, ve raporlanan adres o thread'in kendi `stack_top`'unun
altında olmak zorunda. İki thread'in değerleri farklı olmak zorunda.
Tek bir sayaç yetmez — "RSP0 güncelleniyor" iddiasını, hiç
güncellemeyen bir uygulama da geçirirdi, çünkü ilk değer zaten
doğrudur.

## IST ve ring 3

`#DF` hâlâ IST1'de (ADR 009). IST, ayrıcalık seviyesinden bağımsız
olarak koşulsuz stack değiştirir — yani ring 3'ten gelen bir double
fault da aynı stack'e düşer. Bu doğru davranış: double fault'un sebebi
genellikle stack'in kendisi, ve RSP0'a güvenmek tam da güvenilemeyecek
şeye güvenmek olurdu.

Diğer vektörler RSP0 kullanıyor, M1'deki gibi. Ring 3'ten gelen bir
`#PF` artık gerçekten bir stack değişimi yapıyor — M7'ye kadar
yapmıyordu, çünkü her şey zaten CPL 0'daydı.

## IDT ve DPL

M1'in IDT'si her gate'i DPL 0 ile kurdu ve o karar burada sınanıyor.
DPL 0, ring 3'ten `int n` ile o vektörü **çağırmayı** engeller; `#GP`
alınır. Donanımdan gelen kesmeler ve CPU'nun ürettiği exception'lar
DPL kontrolüne tabi değil, yani ring 3'te bir `#PF` normal şekilde
handler'a gidiyor.

M1'in yorumu "user space kernel exception handler'larını doğrudan
süremez" diyordu ve M8'de bu bir varsayım olmaktan çıkıyor:
`user-privileged` senaryosu ring 3'ten `int3` deniyor ve `#BP` değil
`#GP` bekliyor.

## Elenen alternatifler

**Her yerde `iretq`.** Syscall dönüşünü de `iretq` yapmak, kanoniklik
kontrolünü gereksiz kılardı ve kodu basitleştirirdi. Reddedildi, ama
yakın bir çağrı: `iretq` ~100 çevrim daha pahalı ve syscall sıcak yol.
Asıl sebep eğitimsel — `sysret`'in tuzağını *koymadan* geçmek, M9'un
IPC'sinde aynı tuzağa düşme riskini erteler. Ölçüm testin içinde: iki
yol da koşuluyor ve maliyetleri raporlanıyor.

**`sysenter`/`sysexit`.** 64-bit modda Intel'de çalışır, AMD'de
çalışmaz. Taşınabilirlik `syscall`'da, ve `syscall` zaten 64-bit'in
standart yolu.

**Her thread için ayrı TSS.** RSP0'ı switch'te yazmak yerine `ltr` ile
TSS değiştirmek. `ltr` çok daha pahalı (descriptor okur, busy biti
yazar) ve tek CPU'da hiçbir şey kazandırmıyor. SMP'de tekrar
bakılacak — orada her CPU'nun kendi TSS'i olacak, thread'in değil.

**Kanoniklik kontrolünü atlamak.** "QEMU'da zaten çalışıyor" doğru ve
alakasız: bu bir güvenlik kontrolü ve test ortamında tetiklenmemesi
onu gereksiz kılmıyor. Kontrolün maliyeti üç komut.

## Sonuç

- **İlk giriş `iretq` ile**: beş alan da açıkça yazılıyor, hata
  ayıklanabilir, M1'in çerçeve düzenini kullanıyor.
- **Syscall dönüşü `sysret` ile**, ama RCX kanonik değilse `iretq`'e
  düşerek — CVE-2012-0217 sınıfı bir açık, ve kontrolü test ediliyor.
- **GDT sırası `sysret`'in dayattığı sıra**: kernel CS, kernel SS,
  user CS32, user SS, user CS64. Bir `const` assertion bunu build
  zamanında bağlıyor.
- Slot 5 (user CS32) kullanılmıyor ama **geçerli** bir descriptor
  tutuyor; boş bırakmak sessiz bir `#GP` biriktirmek olurdu.
- **TSS.RSP0 her thread switch'inde yazılıyor**, CR3 ile aynı yerde
  ve aynı gerekçeyle. Test iki thread'in farklı kernel stack'lerine
  girdiğini doğruluyor.
- `#DF` IST1'de kalıyor; ring 3'ten gelen double fault da oraya
  düşüyor ve bu doğru.
- M1'in DPL 0 kararı sınanıyor: ring 3'ten `int3`, `#BP` değil `#GP`.