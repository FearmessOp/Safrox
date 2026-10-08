# ADR 046 — Panic yolu ve kernel'de kalan donanım

Status: accepted (M13)

## Bağlam

M13 seri sürücüyü kernel'den çıkarıyor. Geriye bir soru kalıyor:
**kernel panic ettiğinde ne olacak?**

Panic anında sistem tanımsız durumda. Zamanlayıcıya güvenilmez, IPC
yapılamaz (karşı taraf koşmayabilir), ve sürücü bileşeni ya ölmüştür
ya da panic'e o sebep olmuştur. Ama panic mesajı olmadan hata ayıklama
imkânsız — bu projede 59 senaryonun tamamı seri çıktıya bakarak
denetleniyor.

## Önce: yanlış soru

İlk formülasyon şuydu: "kernel'de seri sürücü kalacak mı, kalmayacak
mı?" Bu soru cevaplanamaz, çünkü iki farklı şeyi tek kelimeye
sıkıştırıyor.

**Bir aygıtı sürmek** şu demek: yapılandırmasına sahip olmak,
kesmelerini karşılamak, durumunu yönetmek, istemciler arasında
hakemlik etmek. Bir UART sürücüsü baud hızını belirler, DLAB'ı
çevirir, FIFO'yu kurar, MCR'yi yazar, IRQ 4'ü alır, gelen baytları
tamponlar.

**Bir porta bayt yazmak** bunların hiçbiri değil. İki yazmaca
dokunur: LSR'yi okur (yer var mı?), THR'ye yazar. Hiçbir şeyi
yapılandırmaz, hiçbir kesme beklemez, hiçbir durum tutmaz.

İkisi arasındaki fark **sayılabilir**: biri sekiz yazmaca dokunur,
diğeri ikiye. Bu, kriter 4'ün iddiasını ölçülebilir kılan şey, ve
aşağıdaki kararın tamamı bu ayrıma dayanıyor.

## Seçenekler

### (a) Kernel'de asgari bir acil yazıcı — seçilen

seL4 bunu yapıyor (`seL4_DebugPutChar`, debug derlemelerde). Bedeli:
kernel donanımı tam bırakmıyor.

### (b) Yalnızca QEMU exit kodu + sabit adrese yapı

Temiz. Panic bir yapıyı bilinen bir adrese yazar, QEMU'dan çıkar,
host tarafı belleği okur.

İki sorunu var. **Gerçek donanımda kör**: `isa-debug-exit` QEMU'ya
özgü, gerçek bir makinede panic sessizce asılı kalır. Ve **test
altyapısını yıkar**: 59 senaryonun tamamı `run.py`'nin seri çıktıdaki
satırları eşleştirmesiyle çalışıyor, panic mesajının dosya:satır'ı
dahil. Bunları belleğe yazılan bir yapıya çevirmek, M0'dan beri
biriken tüm test altyapısını yeniden yazmak demek — ve DOKUNMA
listesi `run.py`'nin çözümlemesini korumayı şart koşuyor.

### (c) Sürücüye "panic kanalı"

İşe yaramaz, ve bu brief'in kendi tespiti. Sürücü ölmüş olabilir;
zaten sürücünün ölümü panic'in en olası sebebi. Çalışması için
çalışması gereken bir hata ayıklama yolu, hata ayıklama yolu değil.

Ayrıca IPC yapmak zamanlayıcıyı çalıştırmak demek, ve panic anında
zamanlayıcı güvenilmez durumda olabilir — `sched`'in kendi
invariant'ını bozması panic sebeplerinden biri.

## Karar

(a), ama kapsamı üç duruma bölerek. Fark, "ne zaman yapılandırır"
sorusunda.

### Durum 1 — Boot: yapılandırır (bir kez)

İlk `println!` çalıştığında sistemde hiçbir bileşen yok; root task
bile henüz başlamadı. Birinden yazdırmasını istemek mümkün değil.

Kernel COM1'i bir kez kuruyor (115200 8N1), loopback kendi testini
yapıyor. Bu **altı yazma** ve ömür boyu bir kez.

### Durum 2 — Normal çalışma: yapılandırmaz

Sürücü başladıktan sonra kernel'in çıktısı yalnızca LSR okur, THR
yazar. LCR'ye, IER'ye, FCR'ye, MCR'ye, divisor'a **dokunmaz**.

Bu bir incelik değil, bir zorunluluk: sürücü aygıtın sahibi ve
yapılandırmayı o belirliyor. Kernel araya girip baud hızını
değiştirseydi, sürücünün sahipliği sözde olurdu.

### Durum 3 — Panic: yapılandırır, ve bu kasıtlı bir istisna

Panic anında sürücünün ölü olduğu varsayılıyor — çoğu zaman
doğrudur, ve yanlış olduğunda da zararı sınırlı, çünkü panic'ten
sonra sistem zaten duruyor.

Burada kernel UART'ı **yeniden kuruyor**: DLAB'ı temizliyor, 8N1'i
geri koyuyor, FIFO'yu boşaltıyor. Sebep somut bir tehlike:

> Sürücü kendi `init`'i sırasında DLAB'ı set etmişken ölürse, UART
> "divisor latch" modunda kalır. O durumda THR'ye yazılan her bayt
> baud bölücüsüne gider ve **tek bir karakter bile çıkmaz**. Panic
> mesajı, onu en çok gerektiğimiz anda kaybolur.

Yani panic yolu nazik olmayı bırakıyor. Bu, yukarıdaki "kernel
yapılandırmaz" kuralının bilinçli ve yazılı istisnası — ve istisnanın
koşulu "sistem zaten bitti" olduğu için, sahiplik iddiasını
zedelemiyor.

## Kernel'de kalan donanım — tam döküm

Kriter 4 bu sayıyı istiyor. Aşağıdaki liste `grep`'le üretildi
(`in`/`out` komutları ve aygıt belleğine `volatile` erişimler) ve
`driver-test` bunu çalışma anında rapor ediyor.

### Çevresel aygıt sürücüsü: SIFIR

M12'de bir tane vardı (COM1). M13'ten sonra yok.

### Kernel'in zorunlu olarak dokunduğu donanım: üç kalem

**1. LAPIC** — `kernel/src/apic.rs`, MMIO.

Zamanlayıcının zaman kaynağı ve her kesmenin EOI'si. Bir
mikroçekirdek bunu devredemez: zamanlayıcı kernel'de (ADR 024) ve
EOI kesme yolunun parçası (ADR 042). LAPIC bir çevre birimi değil,
CPU'nun kendisinin bir parçası — her çekirdeğin içinde.

Devretmek isteseydik, zamanlayıcıyı kesen bileşen zamanlayıcıyı
durdurabilirdi.

**2. I/O APIC** — `kernel/src/ioapic.rs`, MMIO.

Kesme yönlendirme ve maskeleme. ADR 042'nin kontratı — `Ack`
gelmeden ikinci teslimat yok — bu çipin yönlendirme tablosuyla
uygulanıyor. IRQ sahipliği bir capability (ADR 041), ama hattı
fiziksel olarak susturan şey bu.

Bu da bir çevre birimi değil: hangi kesmenin hangi vektöre gideceğine
karar veren mekanizma. Bir bileşene verilseydi, o bileşen başka bir
bileşenin kesmelerini kendine yönlendirebilirdi.

**3. 8259 PIC** — `kernel/src/apic.rs`, iki `outb`.

Boot'ta **bir kez**, ikisini de tamamen maskelemek için (`0xFF`).
Sürülmüyor; susturuluyor. APIC moduna geçerken eski denetleyici
sahte kesme üretmesin diye. Toplam iki yazma, ömür boyu.

### Test koşumu: bir kalem

**4. `isa-debug-exit`** — `kernel/src/qemu.rs`, tek `outl`.

Gerçek donanımda yok. QEMU'dan çıkış kodu ile çıkmanın yolu, yani
test koşucusunun sonucu okuyabilmesinin tek yolu (ADR 002). Bir aygıt
sürücüsü değil; bir çıkış mekanizması.

### Acil konsol: bir kalem, ve sınıfı farklı

**5. COM1** — `kernel/src/emergency.rs`.

Yukarıdaki üç duruma göre: boot'ta altı yazma, normalde bayt başına
bir okuma + bir yazma, panic'te yeniden kurulum.

Sürmüyor: IRQ 4'ü almıyor, gelen bayt okumuyor, tampon tutmuyor,
istemci tanımıyor. Aygıtın sahibi sürücü bileşeni; bu yalnızca
"kimseden isteyemediğim anlarda porta bayt yazan" bir yol.

### Rapor satırı

`driver-test` şunu basıyor:

```
[driver] kernel still drives 0 peripheral devices
[driver] kernel hardware: LAPIC (timer+EOI), I/O APIC (routing),
         8259 PIC (masked once at boot), isa-debug-exit (test
         harness), COM1 (emergency console only: LSR+THR, no
         configuration while a driver owns it)
```

Sıfır iddiası **çevresel aygıt sürücüsü** için. Diğer dördü
sayılıyor ve adlandırılıyor, çünkü sayılmayan bir kalem "sıfır"
iddiasını denetlenemez yapardı.

## Bu sayı neyi ispatlıyor, neyi ispatlamıyor

**İspatlıyor:** kernel'de artık hiçbir aygıtın yapılandırması,
kesmesi veya durumu yok. Yeni bir aygıt eklemek kernel'e satır
eklemiyor.

**İspatlamıyor:** kernel'in donanımdan tamamen çıktığını. Çıkmadı ve
çıkamaz — zamanlayıcısı bir timer'a, kesme yolu bir denetleyiciye
dayanıyor. "Mikroçekirdek donanım tanımaz" ifadesi, "mikroçekirdek
**çevre birimi** tanımaz" olarak doğru; mutlak hali hiçbir
mikroçekirdekte doğru değil, seL4'te de değil.

Acil konsol da bir ödün ve öyle sayılıyor. Onsuz bir panic sessiz
olurdu, ve sessiz panic'le hata ayıklanamaz.

## 59 senaryo ne oluyor

Hepsi çalışmaya devam ediyor, ve bunun sebebi kararın şekli:
`println!` aynı makro, aynı çıktı, aynı `run.py` desenleri. Değişen
tek şey arkasındaki modülün adı (`serial` → `emergency`) ve
sürücünün ne yaptığı değil, kernel'in **ne yapmayı bıraktığı**.

Bu bir tesadüf değil, (b)'yi eleme gerekçesiydi: test altyapısını
yıkmayan tek seçenek buydu.

## Bilinen açık

Sürücü `init` sırasında DLAB set iken kernel bir satır basarsa, o
satır kaybolur. Pencere mikrosaniyeler ve yalnızca sürücü
başlatılırken var. Kapatmanın yolu kernel'in DLAB'ı kendi kontrol
etmesi olurdu — ki bu tam olarak "yapılandırmaz" kuralını bozmak
demek. Pencereyi kapatmak yerine **yazıyoruz**, çünkü ödünün hangi
yöne verildiği görünür olmalı. [`docs/hardware-todo.md`](../hardware-todo.md)'ye eklendi.