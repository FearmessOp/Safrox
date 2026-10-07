# ADR 041 — IRQControl ve IRQHandler: bir kesme hattının tek sahibi

Status: accepted (M10)

Kaynak: seL4 Reference Manual (v13), bölüm 8 ("Hardware I/O"), özellikle
8.1 (`IRQControl`) ve 8.2 (`IRQHandler`).

ADR 017 nesne kümesine sekizinci bir tip koydu — `IRQHandler` — ve onu
M10'a bıraktı. ADR 038 Notification'ı "bir kesme işleyicisi bloke
olamaz" gerekçesiyle tasarladı ve M12'nin sürücüsünü işaret etti. Bu
ADR ikisini birleştiriyor: **bir donanım kesmesi user-space'teki bir
thread'e nasıl ulaşıyor.**

## Soru

Bir kesme geldiğinde kernel ne yapmalı, ve hangi bileşen onu almaya
yetkili?

İkinci yarısı birincisinden daha zor. "Kesme geldi, user'a ilet"
cümlesi bir yetki sorusunu saklıyor: **hangi** user'a? Bir bileşen
başka bir bileşenin cihazının kesmesini dinleyebilir mi?

## Karar: iki tip, bir türetme zinciri

```
IRQControl  (tek, boot'ta init'e verilir)
    |
    +-- IRQHandler(irq=5)   -> bir Notification'a bagli
    +-- IRQHandler(irq=11)  -> baska bir Notification'a
```

**`IRQControl`**: "kesme hatlarını dağıtma yetkisi". Sistemde bir
tane, boot'ta üretiliyor ve init bileşenine veriliyor (ADR 022'nin
deseni: kernel kendine kaynak ayırmıyor, hepsini init'e devrediyor).

**`IRQHandler(n)`**: "n numaralı kesme hattının sahibiyim". Yalnızca
`IRQControl` üzerinden türetilebiliyor.

### Neden `IRQHandler` bir nesne, bir hak biti değil

Alternatif, `Notification`'a bir "şu IRQ'ya bağlı" alanı eklemekti.
Reddedildi, ve gerekçe ADR 019'un derivation ağacı:

Bir IRQHandler **revoke edilebilir olmalı**. Bir bileşen çöktüğünde
tuttuğu kesme hattının serbest kalması gerekiyor, yoksa o hat
sistemin ömrü boyunca ölü kalır. Revoke, capability sisteminin zaten
yaptığı iş — ama yalnızca bir *nesneyi* adlandıran capability'ler
için. Notification'ın içindeki bir alan revoke edilemez; revoke
edilecek ayrı bir şey yok.

Nesne olmasının ikinci sonucu: `IRQControl`'den türeyen bir
`IRQHandler`, `IRQControl` revoke edilirse **o da gidiyor**
(ADR 019'un preorder taraması). Yetkiyi veren geri alabiliyor.

## Bir IRQ'nun tek sahibi var

```
IRQControl.Get(irq, dest_slot):
    irq gecerli mi             -> degilse InvalidArgument
    irq zaten sahipli mi       -> ise IrqAlreadyOwned
    dest_slot bos mu           -> degilse SlotNotEmpty
    IRQHandler(irq) yaz
```

İkinci kontrol bu ADR'nin asıl kararı. İki bileşen aynı hattı
alabilseydi:

- **Kesme kime gider?** İkisine birden göndermek, ikisinin de aynı
  cihazı yönettiğini varsaymak demek. Sırayla göndermek bir
  zamanlayıcı politikası, ve kernel'de politika olmaz (ADR 011).
- **Ack'i kim yapar?** Biri Ack ederken diğeri hâlâ işliyorsa, hat
  erken açılıyor ve ikinci bileşen yarıda kesiliyor.
- **Daha kötüsü:** B bileşeni, A'nın cihazının kesmelerini dinleyerek
  A hakkında bilgi sızdırabilir. Bir disk sürücüsünün kesme hızı, o
  diske ne yazıldığı hakkında bir yan kanal.

Sahiplik kernel'de bir tabloda tutuluyor: `irq -> IRQHandler nesnesinin
fiziksel adresi, ya da 0`. Tablo sabit boyutlu (`MAX_IRQ` giriş),
çünkü dinamik olsaydı bir allocation olurdu ve ADR 022 kesme
bağlamında allocation'ı yasaklıyor — tablo kesme yolunda **okunuyor**.

### Hat, handler öldüğünde serbest kalıyor

`IRQHandler` revoke edildiğinde kernel tablodaki girişi sıfırlıyor ve
hattı **maskeliyor**. Serbest kalan hat yeniden `Get` ile alınabiliyor.

Maskeleme ihmal edilemez: sahipsiz bir hat açık kalırsa, cihaz kesme
göndermeye devam eder ve her biri kernel'e girip "bu hattın sahibi
yok" diye dönmek zorunda kalır. Bir kesme fırtınası, hiçbir işe
yaramayan bir fırtına.

## Kesme → Notification yolu

```
IRQHandler.SetNotification(ntfn_cap):
    ntfn_cap'in bulundugu SLOT'un adresi + o slotun generation'i
    yaziliyor
```

Kesme geldiğinde kernel o Notification'ı `Signal` ediyor — ADR 038'in
`Signal`'i, aynı kod. Badge, hangi kesmenin geldiğini söylüyor: bir
thread birden çok IRQHandler'ı tek bir Notification'a bağlayabilir ve
maskeden hangisinin ateşlediğini okuyabilir.

Bu, ADR 037'nin badge'i maske yapma kararının ikinci meyvesi
(birincisi ADR 038'in "bir thread bir Notification'a bağlanır" kuralı).

### Nesnenin değil, *slotun* adresi — uygulama sırasında değişti

Bu ADR'nin ilk hâli "Notification'a bir işaretçi ve bir badge kopyası"
diyordu. Kodu yazarken üçü birden daha iyi olan bir alternatif çıktı:
capability'nin bulunduğu slotun adresi, yanında o slotun
generation'ı — ADR 018'in CPtr'lerinde, ADR 028'in VSpace'lerinde ve
ADR 036'nın reply hedeflerinde kullanılan aynı deyim.

Üç sonucu var:

- **Badge capability'den okunuyor**, her kesmede. Sürücü farklı
  badge'li bir capability mint ederse kesmeleri o badge ile geliyor;
  kernel'e ikinci kez söylemesi gerekmiyor. Handler'da bir badge
  alanı, aynı şeyi söyleyen ikinci bir yer olurdu.
- **Notification capability'si silinince kernel bunu *anlıyor***,
  kendisine söylenmesi gerekmiyor: `delete` zaten slotun
  generation'ını ilerletiyor. `delete`'in borçlu olduğu bir defter
  tutma yok, dolayısıyla unutulabilecek bir şey de yok.
- **Sinyal, thread'in çağırdığı `signal`'in ta kendisinden geçiyor.**
  Kopyası değil: bir cihazın ürettiği notification ile bir thread'in
  ürettiği birbirinden ayırt edilememeli, ve bunu garanti etmenin en
  ucuz yolu tek bir tanesinin olması.

`irq-revoked` senaryosu bunu kuruyor: Notification *nesnesi* yaşarken
handler'ın işaret ettiği capability siliniyor. Nesne hâlâ aynı
adreste, yani yalnızca adres saklayan bir uygulama sinyali yabancıya
teslim ederdi. Generation kontrolü yakalıyor:

```
[irq] detected: stale_notification 0 -> 1, delivered unchanged at 0
```

## `IRQControl` nesnesi olmayan ilk capability — ve bunun bedeli

`IRQControl` bir **nesne adlandırmıyor**. Taşıdığı şey kesme
hatlarını dağıtma yetkisi, ve bir hat bellek değil. Diğer her tip bir
Untyped'dan kesilmiş belleği adlandırıyor; bunun işaret edeceği bir
şey yok.

Bu, uygulamada hemen bir çelişki çıkardı. `Slot::is_empty()`,
**nesne adresinin** sıfır olup olmadığına bakıyordu — ve boot'ta
tohumlanan IRQControl'ün adresi null olduğu için ilk koşuda
`irq-test` "IRQControl.Get refused the test line: EmptySlot" diyerek
paniklendi. Orada olduğu apaçık olan bir capability "boş" okunuyordu.

İki tanım çakışmıştı, ve hangisinin kastedildiğini ADR 017 zaten
yazmıştı:

> "`Null` dokuzuncu bir tip değil: bir capability'nin *yokluğu*, ve
> burada olmasının sebebi bir slotun ayrı bir 'dolu' bayrağına
> ihtiyaç duymadan her zaman geçerli bir `CapType` taşıması."

Tip, doluluk bayrağının kendisi. Adrese bakmak tutarsızlıktı — ve
dokuz tip için aynı cevabı verdiği için dokuz milestone boyunca
görünmedi. `is_empty` artık tipe bakıyor; eski tipler için ikisi
birebir aynı, yani mevcut hiçbir slotun cevabı değişmiyor.

Kaydetmeye değer olan kısmı: hata, yeni koddaki bir yanlış değildi.
Dokuz tipin tesadüfen örtüştüğü iki tanımın onuncu tiple ayrışmasıydı.

### Neden Notification, Endpoint değil

`Signal` bloke olmuyor. Bir kesme işleyicisi bloke **olamaz**: bloke
olacak bir thread yok, kernel kesme bağlamında ve dönecek bir yeri
yok.

ADR 038 bu gerekçeyi Notification'ın var olma sebebi olarak yazmıştı;
M10 onu kullanan ilk şey.

## Elenen alternatifler

**Kesmeyi doğrudan bir thread'e teslim etmek** (upcall). Kernel'in
thread'in yığınına bir çerçeve itmesi gerekirdi, ve o thread o anda
ne yapıyorsa yarıda kesilirdi. Sinyal semantiği — ve sinyaller, yeniden
girişlilik sorunlarının klasik kaynağı.

**IRQHandler'ı Notification'ın bir alanı yapmak.** Revoke edilemez;
yukarıda.

**Bir IRQ'yu birden çok bileşenin paylaşması.** Üç ayrı sorun,
yukarıda. Paylaşılan hatlar gerçek donanımda var (PCI INTx) ve
cevabı M12'nin sorunu: bir "IRQ multiplexer" bileşeni, tek sahip
olarak hattı alıp kendi istemcilerine dağıtır. Kernel'de değil,
user-space'te — ve bu tam olarak mikroçekirdek argümanı.

**Kesme numarası yerine vektör numarası kullanmak.** Vektör bir CPU
kavramı, IRQ bir yönlendirme kavramı; ikisi arasındaki eşleme
kernel'in işi ve user'ın bilmesi gereken bir şey değil.

## Ölçülen

```
[irq] derivation: IRQ 5 claimed and masked; a second claim on it gives
      IrqAlreadyOwned and leaves no trace; IRQ 11 claimed independently
```

"Leaves no trace" ayrı bir iddia: reddedilen `Get`'ten sonra hattın
sahipsiz **ve** hedef slotun boş olduğu ayrıca kontrol ediliyor. Hattı
alıp capability yazmayan bir `Get`, ikisini birden yapmayandan daha
kötü olurdu — hat kalıcı olarak alınamaz hâle gelirdi.

## Sonuç

- **İki tip**: `IRQControl` (tek, init'te) ve ondan türeyen
  `IRQHandler(n)` (hat başına bir tane).
- **Bir hattın tek sahibi var.** İkinci `Get` `IrqAlreadyOwned`
  veriyor. Gerekçe üç katlı: kesme kime gider, Ack'i kim yapar, ve
  bir bileşen başkasının kesme hızını yan kanal olarak kullanamaz.
- `IRQHandler` bir **nesne**, çünkü revoke edilebilir olması
  gerekiyor — ve revoke, capability sisteminin zaten yaptığı iş.
  `IRQControl` revoke edilirse altındaki bütün handler'lar gidiyor
  (ADR 019).
- Handler ölünce hat **serbest kalıyor ve maskeleniyor**. Sahipsiz
  açık bir hat, hiçbir işe yaramayan bir kesme fırtınası demek.
- Kesme bir **Notification'ı sinyalliyor** (ADR 038), badge ile.
  Birden çok hat tek Notification'a bağlanabiliyor; maske hangisinin
  ateşlediğini söylüyor (ADR 037).
- Handler, Notification'ın değil **capability'nin bulunduğu slotun**
  adresini ve generation'ını tutuyor (ADR 018'in deyimi). Badge her
  kesmede capability'den okunuyor, silinen bir capability tespit
  ediliyor, ve sinyal thread'in kullandığı `signal`'in aynısından
  geçiyor.
- `IRQControl` **nesnesi olmayan ilk capability**. Bu, `is_empty`'nin
  adrese değil tipe bakması gerektiğini ortaya çıkardı — ADR 017'nin
  zaten söylediği şey, dokuz tip boyunca farkı görünmediği için
  fark edilmemişti.
- Sahiplik tablosu **sabit boyutlu**, çünkü kesme yolunda okunuyor ve
  ADR 022 orada allocation'ı yasaklıyor.
- Paylaşılan hatlar kernel'de çözülmüyor: tek sahip alır, user-space'te
  bir multiplexer dağıtır. **M12.**