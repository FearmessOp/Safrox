# ADR 049 — BootInfo'da framebuffer, ve sürümün artması

- **Durum:** Kabul edildi
- **Tarih:** M14
- **Bağlam:** ADR 013 (reclaim), ADR 044 (BootInfo protokolü), ADR 050
  (device Untyped tohumlama)

## Bağlam

Limine, çekirdeğe bir framebuffer verir: fiziksel adres, genişlik,
yükseklik, pitch, piksel başına bit. M14 bu yüzeyi bir user bileşenine
devrediyor; bileşenin onu bulabilmesi için geometrinin BootInfo'dan
geçmesi gerekiyor.

İki ayrı sorun var ve ikisi de burada karara bağlanıyor: bilginin
**ne zaman** okunacağı, ve BootInfo'ya alan eklemenin **ne anlama
geldiği**.

## Karar 1: response boot'ta kopyalanır

ADR 013 kuralı açık — bootloader-reclaimable bellek geri kazanıldıktan
sonra Limine'in response yapıları okunamaz. Framebuffer response'u o
belleğin içinde durur.

Dolayısıyla geometri, `reclaim` çağrılmadan **önce** çekirdeğin kendi
`static`'ine kopyalanır. Bu, M3'ün HHDM ve memmap için yaptığının
aynısı; yeni bir desen değil, mevcut desenin bir alanı daha.

Kopyalanan şey yalnızca beş sayı ve bir adres. Framebuffer'ın
*içeriği* kopyalanmaz — o zaten reclaimable bellekte değil, aygıtın
kendi belleğinde.

## Karar 2: BOOTINFO_VERSION 1 → 2

Alan eklemek ABI değişikliğidir. ADR 044'ün kuralı: **alanlar sona
eklenir, asla araya sokulmaz veya yeniden numaralanmaz.** M13'ün
`ioport_control_slot`'u `lost_bytes`'tan sonraya konmuştu, tam da bu
yüzden.

Framebuffer alanları da sona ekleniyor, `scenario`'dan sonra. Ama bu
sefer sürüm de artıyor, ve fark şu:

**M13'te eklenen alan, eski bir root task için anlamsızdı ama
zararsızdı.** `ioport_control_slot`'u okumayan bir bileşen, onu
okumadığı için hiçbir şey kaybetmiyordu.

**M14'te eklenen alan bir capability'ye işaret ediyor.** Framebuffer
device Untyped'ı, root task'ın Untyped listesinin *içinde* duruyor
(ADR 050) ve onu ayırt eden tek şey bu alan. Alanı bilmeyen bir root
task, framebuffer'ı sıradan bir Untyped sanıp ondan TCB üretmeye
çalışır — ve `DeviceUntypedFrameOnly` ile reddedilir. Reddedilme
doğru davranış, ama bileşen *neden* reddedildiğini bilmez ve
"belleğim bitti" diye yorumlar.

Sürüm artışı bunu "sessiz yanlış yorum"dan "açık uyumsuzluk"a
çeviriyor. Root task `version != BOOTINFO_VERSION` görünce durur ve
söyler.

### `header_size` neden yetmiyor

`header_size` zaten var ve eski bir okuyucunun descriptor'lara doğru
atlamasını sağlıyor. Sorulması gereken: o varken sürüme ne gerek var?

Cevap, ikisinin farklı soruları yanıtlaması: `header_size`
*"descriptor'lar nerede başlıyor"* sorusunu yanıtlar ve yanlış
hesaplanmış bir ofset yüzünden çöp okumayı engeller. Sürüm ise
*"okuduğum alanlar bana söylendiği şeyi mi ifade ediyor"* sorusunu
yanıtlar. Birincisi bir yerleşim sorunu, ikincisi bir anlam sorunu.

## Eklenen alanlar

```rust
pub framebuffer_untyped_slot: u32,  // 0 = framebuffer yok
pub framebuffer_width:  u32,
pub framebuffer_height: u32,
pub framebuffer_pitch:  u32,        // BAYT, piksel değil
pub framebuffer_bpp:    u32,
pub framebuffer_address: u64,       // fiziksel
```

**Sıfır "framebuffer yok" demek.** Limine her zaman bir framebuffer
vermez — seri konsoldan başlatılmış bir makinede hiç olmayabilir. Bunu
bir hata saymak yanlış olurdu: M0'dan M13'e kadar her şey ekransız
çalıştı ve çalışmaya devam etmeli. Slot sıfırsa root task ekran
bileşenini kurmaz ve bunu *söyler* — sessizce atlamaz, çünkü sessiz
atlama ile "kurdum ama çalışmadı" ayırt edilemez.

### pitch neden ayrı bir alan

`pitch`, bir satırın baytı. `width * bpp / 8`'e **eşit olmak zorunda
değil**: donanım satırları hizalar, ve QEMU'da eşit olması gerçek
donanımda da eşit olacağı anlamına gelmez.

Alanı ayrı tutmak yetmez — kullanılmasını *zorlamak* gerekir, ve bunu
zorlayan şey testtir. `fb-test` pitch'in doğru kullanıldığını satır
sonu/satır başı sınırından okuyarak denetler (M14 senaryo 5). Mutasyon
denetimi bunun üzerine kurulu ve QEMU'da pitch == width*bpp/8 ise
mutasyonun **hayatta kalacağı** baştan biliniyor; o durumda rapor
edilir, gömülmez (ADR 023).

### bpp bir bit sayısı, bayt değil

Limine bunu bit olarak verir (tipik 32). Bayta çevirmek çağıranın işi
ve bölme tek yerde yapılır. Alanın adı `bpp` çünkü Limine'in adı o;
birimi burada yazılı olduğu için karışmaz.

## Elenen alternatifler

**(a) Geometriyi root task'a bir IPC mesajıyla göndermek.** Root
task'ın konuşacağı kimse yok — o ilk bileşen. Çekirdeğin ona bir
şey söylemesinin tek yolu BootInfo.

**(b) Framebuffer'ı ayrı bir sayfaya koymak.** BootInfo zaten bir
sayfa ve 128 descriptor'dan sonra yer var. İkinci bir sayfa, ikinci
bir eşleme ve ikinci bir adres sabiti demek olurdu.

**(c) Sürümü artırmamak, yalnızca alan eklemek.** M13'te yapılan bu
ve orada doğruydu. Burada değil: yukarıdaki gerekçe, eklenen alanın
bir capability'yi *ayırt ettiği* — onu bilmeyen bir okuyucunun o
capability'yi yanlış kullanacağı.

## Sonuçlar

- `BOOTINFO_VERSION` 2. Root task sürümü kontrol ediyor ve
  uyuşmazlıkta duruyor; bu kontrol M12'den beri var ve ilk kez
  anlamlı bir şey yapabilecek durumda.
- Çekirdek framebuffer geometrisini reclaim'den önce kopyalıyor.
- Framebuffer yokluğu bir hata değil, raporlanan bir durum.
- `pitch` ayrı bir alan ve kullanımı testle zorlanıyor.

---

## M14 eki — ABI'ye sonradan eklenen iki sabit

Uygulama sırasında iki sayı daha `safrox_abi`'ye taşındı. İkisi de
BootInfo alanı değil; ikisi de "kernel ile bileşenin aynı sayıyı
bilmesi gerekiyor" durumuna örnek, ama **gerekçeleri farklı** ve fark
yazılmaya değer.

### `RETYPE_MAX_COUNT` — bileşen bu sınırı bilmek zorunda

[ADR 020](020-retype-semantigi.md) `count <= 256` koydu: tek bir
syscall'ın en kötü durumu sınırlı kalsın. Sayı kernel'de bir sabitti.

Ekran bileşeni bin Frame istiyor. İlk sürümü hepsini tek çağrıda
istedi, `TooManyObjects` aldı, ve **bunu bir bellek kıtlığı sandı** —
çünkü retry döngüsü her hatayı yarıya inerek yanıtlıyordu, 1'e kadar
inip "bu Untyped tükenmiş" sonucuna varıyordu.

Asıl sorun döngünün hatayı ayırt etmemesiydi (düzeltildi), ama altında
yapısal bir şey var: **bir bileşen bu sınırı keşfedemez.** Hata kodu
"çok fazla" diyor, "en fazla şu kadar" demiyor. Keşfetmenin tek yolu
ikili arama, yani sınırı öğrenmek için sınıra çarpmak.

Dolayısıyla sayı ABI'ye taşındı ve kernel oradan okuyor:

```rust
pub const RETYPE_MAX_COUNT: usize = safrox_abi::RETYPE_MAX_COUNT as usize;
```

İki kopya değil, bir tanım. Bugün uyuşan iki kopya, bir yeniden
numaralandırma sonrası bileşenin tam bir fazla istemesine yeter.

### `FRAMEBUFFER_BASE` — bu bir taviz, ve öyle yazılıyor

Ekran bileşeni framebuffer'ı kendi adres uzayında 16 MiB'a eşliyor.
**Bu adres kernel'i hiç ilgilendirmez**: bileşen kendi VSpace'ini
tutuyor ve nereye ne koyacağına kendi karar veriyor
([ADR 043](043-user-space-vspace-yonetimi.md)). Kernel'de o sabiti
okuyan hiçbir satır yok.

Okuyan tek şey `fb-out-of-bounds` senaryosu, ve sebebi bir zamanlama
olgusu:

> Bileşenin son iki eylemi, adresi `Echo` ile bildirmek ve hatalı
> yazmayı yapmak. Arada **hiçbir zamanlama noktası yok** — `Echo`
> doğrudan ring 3'e dönüyor ([ADR 048](048-syscall-donus-yolunda-thread-basi-durum.md)),
> ve bir sonraki komut makineyi durduruyor. Raporu bekleyen kernel
> thread'i bir daha hiç koşmuyor.

Yani senaryonun ilk hâli, **üretilmesi imkânsız bir satır** istiyordu.
"Eksik satır" hatası bir arıza değil, erişilemezlikti.

Çözüm: kernel adresi kendisi hesaplıyor (bu sabit + geometri), hatadan
**önce** basıyor, ve `#PF` işleyicisinde CR2 ile karşılaştırıyor.
Bu, echo'dan daha güçlü bir test — tahmin ile gözlem artık iki ayrı
yerden geliyor, oysa echo yalnızca bileşenin aritmetik yapabildiğini
gösterirdi.

Sabitin ABI'de olması yine de bir taviz: bir bileşenin iç kararı,
onu sınayabilmek için paylaşılan bir yere kondu. Alternatif — bileşenin
adresi bir Endpoint üzerinden bildirmesi — bir protokol icat etmek
demekti, ve o protokolün tek müşterisi bir test olurdu. Taviz
yazılıyor ki bir sonraki okuyan onu bir tasarım sanmasın.