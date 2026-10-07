# ADR 042 — Kesme yolu: kernel'in yaptığı dört şey, ve Ack'e kadar maskeli

Status: accepted (M10)

Kaynak: seL4 Reference Manual (v13), 8.2 (`IRQHandler_Ack`); Intel SDM
Vol. 3A, Chapter 11 ("APIC"), özellikle 11.8.4 (EOI) ve 11.5.1 (LVT).

ADR 041 kesmenin **kime** gideceğini karara bağladı. Bu ADR ne olduğunu:
kesme geldiği andan user-space thread'in uyandığı ana kadar kernel'in
yürüttüğü kod.

## Karar: üç adım, ve dördüncüsü yok

```
kesme geldi (vektor V):
    1. hattin sahibini bul       (tablo okumasi, O(1))
    2. hatti maskele             (Ack'e kadar kapali)
    3. Notification'i sinyalle   (ADR 038'in Signal'i)
    4. LAPIC'e EOI               (CPU'nun oncelik kapisini ac)
    don
```

Master prompt'un anti-pattern listesinde bu açıkça yazılı: **kesme
yolunda başka hiçbir mantık olmayacak.** Cihaz yok, tampon yok,
"küçük bir optimizasyon" yok.

Kuralın gerekçesi, kuralı yazmaktan daha önemli:

- **Bu kod kesme bağlamında koşuyor.** Bloke olamaz, allocate edemez
  (ADR 022), ve koştuğu süre boyunca o öncelikteki her kesme
  gecikir.
- **Bu kod user'ın kontrolündeki bir hızda koşuyor.** Kesme hızını
  cihaz belirliyor; kötü yapılandırılmış ya da kasten kötü niyetli
  bir cihaz saniyede yüz binlerce kesme üretebilir. Buradaki her ek
  komut, o çarpanla çarpılıyor.
- **Bu kod kernel'de, yani güvenilir tabanda.** Cihaz mantığı
  user-space'te olmalı — mikroçekirdek argümanının tamamı bu.

### Maskeleme neden 3'ten önce

Sıra önemli. Notification sinyallendiği anda bekleyen thread
*hazır* oluyor; bir sonraki zamanlayıcı kararında koşabilir. Eğer
hat o ana kadar maskelenmemişse, cihaz ikinci bir kesme gönderebilir
ve işleyici thread daha ilkini işlemeye başlamadan ikinci sinyali
almış olur.

Sinyal bir maske olduğu için (ADR 038) ikinci sinyal kaybolmuyor —
ama bilgi kayboluyor: thread "bir kesme geldi" ile "iki kesme geldi"
arasını ayıramıyor. Sürücü için bunun anlamı, cihazın durum
yazmacını iki kez okuması gereken bir durumu bir kez okuması.

Önce maskele, sonra sinyalle. İki satır, ve aralarındaki sıra bir
karar.

## Ack: hat, işleyici bitirdiğini söyleyene kadar kapalı

```
IRQHandler.Ack():
    hattin maskesini kaldir
```

Hepsi bu. Ama ne zaman çağrıldığı sürücünün işi, ve kernel'in bunu
zorlamaması bir karar:

**Kernel, Ack'in ne zaman geleceğini bilmiyor ve bilmemeli.** Sürücü
cihazın durum yazmacını okuyup kesmeyi "gerçekten" temizlediğinde
Ack ediyor. Kernel o yazmacın nerede olduğunu bilmiyor — bilseydi,
cihaz mantığı kernel'de olurdu.

### Ack gelmezse

Hat kapalı kalır. O cihazdan başka kesme gelmez.

**Bu bir kilitlenme değil, bir yalıtım.** Çöken bir sürücü kendi
cihazını susturuyor; sistemin geri kalanı etkilenmiyor. Alternatif —
bir zaman aşımından sonra kernel'in kendiliğinden maskeyi kaldırması
— iki şeyi gerektirirdi: bir zamanlayıcı bütçesi (ADR 017 kapsam
dışı bıraktı) ve kernel'in "sürücü ne kadar sürmeli" hakkında bir
politikası (ADR 011 yasaklıyor).

Sürücü çökerse `IRQHandler`'ı revoke edilir ve hat serbest kalır
(ADR 041). Kurtarma yolu bu, zaman aşımı değil.

### Ack olmadan ikinci kesme gelmiyor — ve bu ölçülüyor

`irq-test` senaryo 4 iki sayaç okuyor: maskeliyken gelip reddedilen
kesmeler (`while_masked`) ve teslim edilenler (`delivered`). Maskeli
hatta kesme yükseltiliyor; birincisi artmalı, ikincisi **artmamalı**.

İki sayacın ikisi de gerekli, ve bu bir titizlik değil: tek sayaç
"maske çalıştı" ile "kesme hiç gelmedi" arasını ayıramaz. Senaryo
bir süre bekleyip "bir şey olmadı" demiyor — `while_masked`'ın
*artmasını* bekliyor, yani kesmenin geldiğinin ve reddedildiğinin
olumlu kanıtını topluyor.

Ölçülen (M10, QEMU TCG):

```
[irq] masked: an interrupt on the shut line was refused
      (while_masked 0 -> 1) and not delivered (delivered stayed at 1)
[irq] ack:    the identical stimulus was delivered once the line was
      reopened (1 -> 2 delivered)
```

İkinci satır birincisi kadar önemli: onsuz, hattı kalıcı olarak
maskeleyen bir kernel de testi geçerdi. O bir yalıtım değil, ölü bir
cihaz olurdu.

### K5 mutasyonu: `Ack`'i no-op yapmak

Koşuldu. Üç şey öğretti ve yalnızca birincisi beklenendi.

**Mutasyon öldü.** `Ack` maskeyi hiç kaldırmazsa koşu başarısız
oluyor. Sözleşme test ediliyor.

**İlk yakalama yanlış yerden geldi.** Senaryo 3'ün
`assert!(!is_masked(..))` satırı önce patladı ve koşuyu senaryo 4'e
varmadan durdurdu. O assert, **`Ack`'in yazdığı bayrağı okuyor** —
mutasyona "kendini mutasyona uğrattın mı" diye sormaya yakın bir
şey. Tek kanıtı bu biçimde olan bir test takımı K5'in lafzını
karşılar, ruhunu değil.

**Davranışsal kontrol tek başına yetiyor.** Bunu görmek için iki
`is_masked` assert'i geçici olarak kaldırılıp mutasyon yeniden
koşuldu. Hata bu kez şuradan geldi:

```
[PANIC] after Ack the same interrupt was still not delivered;
        the line is masked permanently, which silences the device
        rather than isolating it
```

Kesme yükseltildi, teslim gelmedi, sayaç bunu söyledi — hiçbir
bayrak okunmadan. Sahip olmaya değer kanıt bu.

Mutasyonun bir varyantı da gerekti: "hattı hiç açma" senaryo 4'ün
ikinci yarısına varamadan yakalanıyor, bu yüzden `Ack` tam bir kez
çalışacak şekilde değiştirildi. O hâlde birinci yarı geçti, ikinci
yarı patladı — yani iki yarı farklı şeyleri test ediyor, aynı şeyi
iki kez değil.

`is_masked` assert'leri yerinde bırakıldı: hızlı ve ismiyle konuşan
bir hata mesajı değerli. Ama kontrolü sağlam yapan şey onlar değil,
ve bu not sonraki okuyucu onları öyle sanmasın diye burada.

## Revoke edilmiş bir handler'a kesme gelirse

"Tanımsız" bir cevap değil (master prompt). Üç seçenek vardı:

**(a) Kernel panikler.** Reddedildi, ve gerekçesi ADR 030'un kuralı:
kernel user'ın hatası yüzünden durmaz. Üstelik bu bir user hatası
bile değil — cihaz, sürücüsünün revoke edildiğini bilmiyor.

**(b) Kesme sessizce yutulur, hat açık kalır.** Reddedildi: sahipsiz
açık bir hat, her biri hiçbir şey yapmadan dönen bir kesme
fırtınası demek (ADR 041).

**(c) Kesme yutulur ve hat maskelenir.** **Seçilen.**

```
kesme geldi, hattin sahibi yok:
    sayaci artir (orphaned_interrupts)
    hatti maskele
    EOI
    don
```

Hat zaten revoke sırasında maskeleniyor (ADR 041), yani bu yol
normalde erişilemez. Yine de yazılı, çünkü bir yarış var: kesme,
revoke tamamlanmadan hemen önce yükselmiş olabilir. O durumda
kernel'in yapacağı şey tanımlı olmalı, ve "tanımlı" demek
"ölçülebilir" demek — `orphaned_interrupts` sayacı o yüzden var.

`irq-revoked` senaryosu tam olarak bu yarışı kuruyor ve kernel'in
ayakta kaldığını gösteriyor.

## Kesme kaynağı: LAPIC self-IPI

M10'un bir cihaza ihtiyacı yok; bir **kesmeye** ihtiyacı var. QEMU'nun
sunduğu cihazları sürmek, M12'nin işini M10'a çekmek olurdu.

Seçilen kaynak: LAPIC'in **Interrupt Command Register**'ı üzerinden
kendine gönderdiği IPI. Bir satır yazmayla gerçek bir kesme
üretiliyor — CPU'nun kesme kapısından geçen, IDT'den dispatch edilen,
EOI isteyen bir kesme. Taklit değil.

Avantajı, testin kesmeyi **ne zaman** istediğine karar edebilmesi.
Bir zamanlayıcıya ya da bir cihazın kendi ritmine bağlı olsaydı,
senaryo 4'ün "Ack'ten önce tam bir kesme" iddiası bir zamanlama
yarışına dönerdi — ADR 023'ün K8'i tam olarak bunu yasaklıyor.

Maskeleme, LVT girişinin mask bitiyle değil, **yazılım tarafında**
yapılıyor: sahiplik tablosundaki bir bayrak. Self-IPI'nin bir LVT
girişi yok (ICR doğrudan vektör gönderiyor), ve gerçek bir
IOAPIC hattının maskelenmesi M12'nin işi. Bayrak aynı sözleşmeyi
uyguluyor: maskeliyken gelen kesme sayılıyor, teslim edilmiyor.

> **Sınır, açıkça:** bu, gerçek bir IOAPIC hattını maskelemiyor.
> Gerçek donanımda maskeli bir hat *cihazı* durdurur; buradaki bayrak
> kesmenin teslimini durduruyor, CPU'ya ulaşmasını değil. M12 IOAPIC
> yönlendirmesini eklediğinde maskeleme oraya iner ve bu bayrak
> gider. Sözleşme — "Ack'e kadar ikinci kesme teslim edilmez" —
> değişmiyor; onu uygulayan katman değişiyor.

## ADR 022'nin tripwire'ı

M6'dan beri `interrupt_depth` sayılıyor ve kesme bağlamında
`allocate()` panik veriyor. O güne kadar tek kesme kaynağı timer'dı.

M10 ikinci bir kaynak ekliyor, ve `irq-alloc` senaryosu tripwire'ın o
yolda da çalıştığını gösteriyor. Aynı kural, farklı giriş — ve bir
korumanın bir yolda çalışıp diğerinde çalışmaması, tam olarak
ADR 023'ün K5'inin aradığı şey.

Fark yalnızca "başka bir yol" değil: timer kesmesi kernel'in
kendisinin. Bir cihaz kesmesi **ne sıklıkta koşacağına cihazın karar
verdiği** bir yol, yani oradaki bir allocate hızını bir saldırganın
belirlediği bir allocate. Kuralın yazılma sebebi tam olarak bu durum,
ve ilk kez burada sınanıyor.

## Gecikme: kesmeden thread'in koşmasına

Geçme/kalma değil; M13'ün sürücüsünün karşılaştırılacağı taban.

Ölçülen aralık, IPI'yi yükselten komuttan uyanan thread'in **ilk
komutuna** kadar: kesme teslimi, stub, dispatcher, sinyal ve bir
bağlam değişimi dahil. Bağlam değişimi içeride, çünkü sürücü onu
yaşıyor — Notification bir thread'i *hazır* yapıyor, koşar yapmıyor.

```
[irq] wake latency: 32 samples, minimum 10752 cycles, mean 26398,
      maximum 274890
```

Minimum raporlanıyor, ortalama değil. Buradaki ortalama çoğunlukla
ölçümü timer'ın kaç kez böldüğünü ölçer; minimum, yolun kendi
maliyetine en yakın şey. M6'nın bağlam değişimi ~2000 cycle
(emülasyon altında) olduğuna göre, kalanın büyük kısmı kesme teslimi
ve dispatcher.

> Emülasyon altında ölçüldü: TSC sanal ve IPI simüle ediliyor. Bu bir
> büyüklük mertebesi, bir ölçüm değil — M6, M8 ve M9'un aynı şerhi.

## Elenen alternatifler

**Kernel'in Ack'i kendiliğinden yapması** (kesme teslim edilir
edilmez maskeyi kaldırmak). Cihaz hâlâ kesme hattını sürerken hat
açılır ve kesme hemen yeniden gelir — sonsuz döngü. Donanımın
kesmeyi bırakması, sürücünün cihazla konuşmasına bağlı.

**Zaman aşımlı otomatik Ack.** Zamanlayıcı bütçesi ve bir politika
gerektirir; ikisi de kapsam dışı. Ve "sürücü ne kadar sürmeli"
sorusunun kernel'de iyi bir cevabı yok.

**Kesme sayısını saymak (maske yerine sayaç).** Taşma ne olacak
sorusu — ADR 038'in Notification için verdiği aynı cevap. Üstelik
burada daha kötü: sayaç kesme bağlamında artıyor ve taşma kontrolü
o yola bir dal daha ekliyor.

**Kesmeyi timer gibi doğrudan dispatcher'da işlemek.** Timer'ın
işleyicisi kernel'in kendi zamanlayıcısını çağırıyor ve bu bir
istisna: zamanlayıcı kernel'de (ADR 024). Bir cihaz kesmesi için
aynı şeyi yapmak, cihaz mantığını kernel'e sokmak demek.

## Sonuç

- Kesme yolunda kernel **dört şey** yapıyor: sahibi bul, hattı
  maskele, Notification'ı sinyalle, EOI. Beşincisi yok, ve bu bir
  kural: o kod kesme bağlamında, user'ın belirlediği bir hızda, ve
  güvenilir tabanda koşuyor.
- **Maskeleme sinyalden önce.** Tersi olsaydı işleyici thread daha
  başlamadan ikinci kesme gelebilir ve "bir kesme" ile "iki kesme"
  ayırt edilemezdi.
- **Ack'e kadar hat kapalı.** Ack sürücünün işi; kernel cihazın
  durum yazmacını bilmiyor ve bilmemeli.
- **Ack gelmezse hat kapalı kalır.** Kilitlenme değil, yalıtım: çöken
  sürücü kendi cihazını susturuyor. Kurtarma `IRQHandler`'ı revoke
  etmek, zaman aşımı değil.
- **Sahipsiz kesme**: yutulur, hat maskelenir, sayılır
  (`orphaned_interrupts`). Panik değil (ADR 030), sessiz açık hat
  değil (fırtına). Normalde erişilemez — revoke zaten maskeliyor —
  ama bir yarış var ve tanımlı olması gerekiyor.
- Kaynak **LAPIC self-IPI**: gerçek bir kesme, ve testin ne zaman
  istediğine karar edebildiği bir kesme (K8).
- Maskeleme şimdilik **yazılım bayrağı**; gerçek IOAPIC maskelemesi
  M12'de gelir ve sözleşme değişmez, uygulayan katman değişir.