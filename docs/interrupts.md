# Kesmeler: donanımdan user-space sürücüye

M10. Bu belge bir kesmenin CPU'ya geldiği andan user-space'teki bir
thread'in uyandığı ana kadarki yolu anlatıyor.

Kararlar iki ADR'de:

- **ADR 041** — kesme *kime* gider: `IRQControl`, `IRQHandler`, ve bir
  hattın tek sahibi olması.
- **ADR 042** — kernel *ne yapar*: dört adım, maskeleme sinyalden önce,
  ve Ack'in sürücünün işi olması.

M9 (docs/ipc.md) Notification'ı "bir kesme işleyicisi bloke olamaz"
gerekçesiyle tasarlamıştı. M10 onu kullanan ilk şey.

## Resmin tamamı

```
     cihaz                kernel                      user-space
    --------   ----------------------------   ---------------------

    kesme  -->  IDT -> stub -> dispatcher
                       |
                       | 1. hattin sahibini bul  (tablo, O(1))
                       | 2. HATTI MASKELE
                       | 3. Notification'i Signal et
                       | 4. LAPIC'e EOI
                       v
                   thread hazir  ------------>  Wait() donuyor
                                                badge = hangi hat
                                                     |
                                                cihazi oku,
                                                kesmeyi temizle
                                                     |
                   hat yeniden acildi  <-------  Ack()
```

Kernel'in yaptığı dört şey ve beşincisi yok. Cihazla konuşan tek
taraf sürücü.

## Nesneler

### `IRQControl`

Kesme hatlarını dağıtma yetkisi. Sistemde **bir tane**, boot'ta
üretilip init bileşeninin CSpace'ine konuyor.

Retype edilemiyor: `untyped::object_layout` onu açıkça reddediyor.
Adlandırdığı şey bellek değil, ve ne kadar Untyped olursa olsun
ikincisi üretilmiyor.

Sistemdeki **nesnesi olmayan tek capability** bu. Bunun bir bedeli
oldu — ADR 041'deki `is_empty` notu — ve nedeni şu: diğer her tip bir
Untyped'dan kesilmiş belleği adlandırıyor, bunun işaret edeceği bir
şey yok.

### `IRQHandler(n)`

"n numaralı hattın sahibiyim." 32 bayt (ADR 017). İçinde:

| alan | ne |
|---|---|
| `irq` | sahip olduğu hat |
| `bound` | bir Notification bağlandı mı |
| `notification_slot` | capability'nin bulunduğu **slotun** fiziksel adresi |
| `slot_generation` | bağlandığı andaki generation (ADR 018) |

Son iki alan birlikte çalışıyor: kernel, Notification nesnesinin
adresini değil, **capability'nin bulunduğu slotun** adresini tutuyor.
Böylece badge her kesmede capability'den okunuyor (ADR 037), silinen
bir capability tespit ediliyor, ve sinyal thread'in kullandığı
`signal`'in ta kendisinden geçiyor.

## İşlemler

### `IRQControl.Get(irq, untyped, dest)`

Bir hattı sahiplenip handler'ını bir slota koyuyor.

```
IRQControl capability'si mi     -> degilse WrongType
WRITE hakki var mi              -> yoksa InsufficientRights
irq < MAX_IRQ mi                -> degilse InvalidArgument
hat zaten sahipli mi            -> ise IrqAlreadyOwned
retype(IrqHandler)              -> Untyped'dan bellek
hatti MASKELI olarak kaydet
```

Sıra önemli: sahiplik kontrolü retype'tan **önce**, böylece
reddedilen bir `Get` hiçbir iz bırakmıyor (ADR 020'nin atomiklik
kuralı). `irq-no-cap` senaryosu bunu ayrıca kontrol ediyor — hat
sahipsiz **ve** hedef slot boş kalıyor.

Hat **maskeli** başlıyor. Henüz Notification bağlamamış bir sahibin
kesmeyi teslim edecek yeri yok; açık bir hat her cihaz
dalgalanmasında bir ret üretirdi.

### `IRQHandler.SetNotification(ntfn)`

Kesmelerin nereye gideceğini söylüyor. Notification'ın `WRITE`
hakkını burada kontrol ediyor, kesme yolunda değil: oradaki bir hak
kontrolü aynı cevabı binlerce kez hesaplamak olurdu.

Hattı **açmıyor**. Onu `Ack` yapıyor, ve ayrım önemli: sürücü
kurulum sırasında bağlanıyor, kesilmeye hazır olduğunda Ack ediyor.

### `IRQHandler.Ack()`

Hattın maskesini kaldırıyor. Tek satır, ve arkasındaki karar şu:
**kernel bunun ne zaman çağrılacağını bilmiyor.** Sürücü cihazın
durum yazmacını okuyup kesmeyi gerçekten temizlediğinde Ack ediyor;
kernel o yazmacın nerede olduğunu bilmiyor — bilseydi cihaz mantığı
kernel'de olurdu.

Ack gelmezse hat kapalı kalır. **Kilitlenme değil, yalıtım**: çöken
bir sürücü kendi cihazını susturuyor. Kurtarma yolu handler'ı revoke
etmek, zaman aşımı değil.

## Kesme yolu

`cap::irq::handle_interrupt`, dispatcher'dan çağrılıyor:

```
1. sahip var mi?        yoksa: say (orphaned), hatti maskele, don
2. hat acik mi?         degilse: say (while_masked), don
3. HATTI MASKELE
4. hedef hala gecerli mi?  (slot bos degil, tip dogru, generation ayni)
                        degilse: say (stale), handler'i cozup don
5. signal(slot)         -- ADR 038'in Signal'i, aynisi
```

EOI dispatcher'da, her vektör için tek yerde.

### Maskeleme neden sinyalden önce

Sinyal, bekleyen thread'i *hazır* yapıyor; dispatcher dönmeden
koşabilir. Hat o ana kadar kapanmamışsa cihaz ikinci bir kesme
gönderebilir, ve sinyal bir sayaç değil maske olduğu için (ADR 038)
sürücü "bir kesme" ile "iki kesme" arasını ayıramaz.

### Tanımlı olan her durum

| durum | ne oluyor | sayaç |
|---|---|---|
| normal | Notification sinyalleniyor | `delivered` |
| hat maskeli | yutuluyor | `while_masked` |
| sahip yok | yutuluyor, hat maskeleniyor | `orphaned` |
| hedef silinmiş | yutuluyor, handler çözülüyor, hat kapalı | `stale_notification` |
| Notification bağlanmamış | yutuluyor, hat kapalı | `unbound` |

Hiçbiri "tanımsız" değil ve hiçbiri panik değil. Kernel, dışındaki
bir şey yüzünden durmaz (ADR 030).

## Vektörler

```
0..31    CPU istisnalari (ADR 006)
32       LAPIC timer (M6)
33..48   kesme hatlari 0..15     <- M10
255      spurious
```

Hat `n`, vektör `33 + n`'de geliyor. Düz bir ofset, tablo değil:
dispatcher kesme bağlamında koşuyor ve bir çıkarma bir aramadan ucuz.
M12'de IOAPIC yönlendirme girişi tam olarak bu aritmetikle
programlanacak.

Her hat için IDT kapısı var, sahibi olsun olmasın. Kapısı olmayan bir
vektöre gelen kesme, kesme teslimi sırasında #GP demek — yani çift
hata. Kapıyla birlikte aynı kesme `handle_interrupt`'a varıyor,
sahipsiz bulunuyor, sayılıp düşürülüyor: ADR 042'nin *tanımlı*
davranışı.

Kapılar DPL 0. User bir thread `int 38` ile cihaz kesmesi
uyduramamalı; bir sürücüyü user'dan sinyallemenin yolu onun
Notification'ına `Signal`, ve o da bir capability istiyor.

## Kesme kaynağı: LAPIC self-IPI

M10'un bir cihaza değil bir **kesmeye** ihtiyacı var. Kaynak, LAPIC'in
Interrupt Command Register'ı üzerinden kendine gönderdiği IPI.

Gerçek bir kesme: aynı IDT girişi, aynı stub, aynı dispatcher, aynı
EOI. Ama **ne zaman geleceğine test karar veriyor**, ve bu bir test
kararı:

- Bir *cihaz* kesmeyi kendi bildiği zaman gönderir. Senaryo 4'ün
  "Ack'ten önce ikinci kesme teslim edilmiyor" iddiası o hâlde bir
  zamanlama yarışına dönerdi — ADR 023'ün K8'i tam olarak bunu
  yasaklıyor.
- `handle_interrupt`'ı doğrudan çağırmak fonksiyonu test ederdi,
  yolu değil. IDT, stub, dispatcher ve kesme derinliği tripwire'ı
  atlanırdı — M10'un hatalarının yaşayacağı yerler tam olarak oralar.

## Sürücünün yazacağı döngü

M13'teki şekli:

```
Wait(ntfn)            // kesmeyi bekle, badge hangi hat oldugunu soyler
cihazi oku
kesmeyi cihazda temizle
Ack(handler)          // hatti yeniden ac
```

`Wait` ring 3'ten çağrılabiliyor: M10 syscall dispatcher'ına
`Method::Wait` ekledi, ve bu **bloke olan ilk invocation**. Önceki
her şey cevap verip dönüyordu; bu, çağıran thread'i bir sinyal
gelene kadar parkediyor.

Özel bir şey gerekmedi: bir syscall çağıran thread'in kendi kernel
yığınında koşuyor (ADR 029), yani bloke olmak bağlamı oraya
kaydediyor ve ring 3'e dönüş thread yeniden koştuğunda oluyor.

`irq-user` senaryosu bunu uçtan uca gösteriyor.

## Senaryolar

| senaryo | ne gösteriyor | çıkış |
|---|---|---|
| `irq-test` | türetme, bağlama, teslim, maske, gecikme | 33 |
| `irq-user` | **ring 3'teki thread kesme alıyor** | 33 |
| `irq-no-cap` | IRQControl olmadan hat alınamıyor | 35 |
| `irq-revoked` | hedef silinmiş, kernel ayakta | 35 |
| `irq-alloc` | ADR 022 tripwire'ı cihaz kesmesinden | 35 |

`irq-no-cap` ve `irq-revoked` **tasarımca** 35 veriyor: kontrolleri
bittikten sonra kasten panikliyorlar, böylece iddiaları panik bekleyen
bir koşuda doğrulanıyor (`thread-kill-running`'in kurduğu düzen).
İkisinde de kernel ayakta; kanıt logda ve run.py o satırları
istiyor.

## Ölçümler

```
[irq] wake latency: 32 samples, minimum 10752 cycles, mean 26398,
      maximum 274890
```

IPI'yi yükselten komuttan uyanan thread'in ilk komutuna kadar: kesme
teslimi, stub, dispatcher, sinyal ve **bir bağlam değişimi**. Bağlam
değişimi içeride çünkü sürücü onu yaşıyor — Notification bir thread'i
hazır yapıyor, koşar yapmıyor.

Minimum raporlanıyor. Buradaki ortalama çoğunlukla ölçümü timer'ın
kaç kez böldüğünü ölçer.

> Emülasyon altında: TSC sanal, IPI simüle. Büyüklük mertebesi, ölçüm
> değil.

## Bilinen sınırlar

**Maskeleme bir yazılım bayrağı.** Gerçek donanımda maskeli bir hat
*cihazı* durdurur; buradaki bayrak teslimi durduruyor, kesmenin
CPU'ya ulaşmasını değil. M12 IOAPIC yönlendirmesini ekleyince
maskeleme oraya iner. **Sözleşme değişmiyor** — "Ack'e kadar ikinci
kesme teslim edilmez" — onu uygulayan katman değişiyor, ve senaryo 4
olduğu gibi kalıyor.

**Paylaşılan hatlar yok.** Gerçek donanımda varlar (PCI INTx) ve
cevap kernel'de değil: tek sahip hattı alır, user-space'teki bir
multiplexer kendi istemcilerine dağıtır. M12.

**16 hat.** `MAX_IRQ` bir tablo boyutu, donanım sınırı değil. Tablo
kesme yolunda okunuyor ve ADR 022 orada allocation'ı yasaklıyor, bu
yüzden derleme zamanında sabit. Büyütmek tek satır; dinamik yapmak
değil.

**Tek CPU.** Sahiplik tablosu atomics ile korunuyor, ama kesme
yönlendirmesi her zaman bu CPU'ya. Çoklu CPU IOAPIC yönlendirmesi ve
IPI'lerle birlikte gelir.