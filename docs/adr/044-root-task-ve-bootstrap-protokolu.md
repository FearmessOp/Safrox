# ADR 044 — Root task ve bootstrap protokolü

Status: accepted (M12)

Kaynak: seL4 Reference Manual (v13), bölüm 2.3 (initial thread) ve
`seL4_BootInfo`; ADR 018 (CPtr/CSpace), ADR 022 (kernel kendine
bellek ayırmaz), ADR 023 (test disiplini), ADR 042 (kesme yolu).

M11 bir bileşenin başka bir bileşen doğurabildiğini gösterdi. Ama o
ebeveyn kernelin test kodunun içindeydi: `build_component` bir kernel
fonksiyonuydu, CPtr'leri kernel hesaplıyordu, programı kernel
yazıyordu. M12 bunu kaldırıyor.

## Soru

Kernel boot'u bitirdiğinde elinde her şey var: bütün Untyped'lar,
IRQControl, init CSpace. Bunları kime, nasıl verir — ve verdikten
sonra geriye ne kalır?

Üç şeyi aynı anda kararlaştırmak gerekiyor, çünkü biri ötekini
belirliyor: **ilk bileşen nereden geliyor**, **elinde ne olduğunu
nasıl öğreniyor**, ve **kernel ne zaman çekiliyor**.

## Karar 1: root task kernel imajına gömülüyor (.rodata)

Üç seçenek vardı.

**(a) initrd.** Ayrı bir arşiv, Limine modülü olarak yüklenir,
kernel içinde ayrıştırılır. Reddedildi: bir arşiv formatı kernel
içinde bir ayrıştırıcı demek, ve o ayrıştırıcı **boot'ta, güvenilir
tabanın içinde, dışarıdan gelen veriyle** çalışır. M12'nin kazanımı
kerneli küçültmek; bir cpio okuyucu eklemek tersi.

**(b) Limine modülü (tek dosya, ayrıştırmasız).** `ModuleRequest`
ile root task'ın ELF'i ayrı bir dosya olarak yüklenir. Ayrıştırıcı
gerekmiyor, ama iki şey getiriyor: ISO'da ikinci bir dosya (ve 56
senaryonun her biri için ISO kuralının değişmesi), ve bir **başarısız
olabilen bağımlılık** — modül yoksa kernel ne yapacak? ADR 002 "tek
binary, çok ISO" dedi ve bunun tek sebebi test edilen şeyin
gönderilen şey olması. Modül bu özelliği zayıflatıyor: ISO'lar artık
iki artefakttan oluşuyor.

**(c) `.rodata`'ya gömme. Seçilen.**

Root task ayrı bir crate olarak derleniyor, çıkan ELF'ten ham ikili
çıkarılıyor, ve kernelin `.rodata`'sına `include_bytes!` ile
giriyor. Kernel imajı tek dosya kalıyor, ADR 002 bozulmuyor, ve
**bağımlılık derleme zamanında çözülüyor**: root task yoksa kernel
derlenmez. Boot'ta "modül nerede" diye bir soru yok, dolayısıyla o
sorunun cevabı da yok.

Maliyeti dürüstçe: kernel imajı root task kadar büyüyor, ve root
task'ı değiştirmek kerneli yeniden linklemek demek. İkisi de M12
ölçeğinde önemsiz — root task birkaç KiB — ve ikisi de bir gün
gerçekten ayrı bileşenler gerektiğinde (b)'ye geçmeyi engellemiyor.

### Neden bir ELF değil, ham ikili

Gömülen şey ELF değil, `objcopy -O binary` çıktısı. Kernelin bir ELF
yükleyicisi yok ve M12'de olmamalı: yükleyici bir *bileşen* işi
(M11'in ebeveyni çocuğunun kodunu kendi yazdı, ADR 043). Kernel tek
bir düz segmenti sabit bir adrese kopyalıyor, o kadar.

Root task'ın linker script'i bu yüzden tek bir yüklenebilir segment
üretiyor ve `.bss`'i yok sayıyor — bss'i olan bir program için
kernelin sıfırlama yapması gerekirdi, ki bu da yükleyici mantığının
başlangıcı. Root task'ın statikleri `.data`'da, ve linker script'te
bir assert bss'in boş olduğunu doğruluyor.

## Karar 2: BootInfo, root task'ın adres uzayında sabit bir sayfada

Root task başlarken elinde ne olduğunu **bilmesi** gerekiyor: kaç
Untyped var, hangi slotlarda, ne kadar büyükler, IRQControl nerede.

Bu bir **ABI**. Bir kez sabitlenir ve sonra değiştirilemez, çünkü
değiştirmek derlenmiş her bileşeni bozar.

```
0x0040_0000   root task'ın kodu (tek segment)
0x0050_0000   root task'ın yığını (bir sayfa, rw)
0x0060_0000   BootInfo (bir sayfa, SALT OKUNUR)
```

Salt okunur, ve bu bir tercih değil: BootInfo kernelin root task
hakkında söylediği şey, root task'ın kendisi hakkında söylediği şey
değil. Yazılabilir olsaydı root task onu değiştirebilir, sonra
okuyabilir, ve kendi yazdığını kernelin söylediği sanabilirdi.
Tutarsızlık değil, **anlam kaybı** olurdu.

Adresler `usertest`'in M8'den beri kullandıklarıyla aynı hizada
(kod 4 MiB, yığın 5 MiB), ve bu tesadüf değil — aynı yerde olmaları
ring 3'e giriş yolunun tek bir şekli olmasını sağlıyor.

### Format

```rust
#[repr(C)]
struct BootInfo {
    magic: u64,            // 0x53616672_42495631 "SafrBIV1"
    version: u32,
    header_size: u32,      // bu yapının boyutu
    untyped_count: u32,
    untyped_first_slot: u32,   // leaf içindeki ilk indeks
    irq_control_slot: u32,
    root_cnode_slot: u32,      // kendi CNode'u, kendi CSpace'inde
    tcb_slot: u32,
    vspace_slot: u32,
    free_slot_first: u32,      // buradan sonrası boş
    free_slot_end: u32,
    usable_bytes: u64,         // boot'ta sayılan toplam
    untyped: [UntypedDesc; N], // header_size'dan sonra
}

#[repr(C)]
struct UntypedDesc {
    base: u64,       // fiziksel adres
    size_bits: u8,
    is_device: u8,
    _pad: [u8; 6],
}
```

**`magic` ve `version` ilk iki alan**, ve sırası önemli: yanlış
sürümdeki bir root task'ın okuyabileceği tek şey bunlar olmalı.
Sonra gelen her alanın anlamı sürüme bağlı; bu ikisinin anlamı
değil.

**`header_size`** ileriye dönük alan ekleme içindir. Bir gün
BootInfo büyürse, eski bir root task başlığı bu boyuta göre atlar ve
tanımadığı alanları okumaz.

**`usable_bytes`** senaryo 3'ün dayanağı. Kernelin boot'ta saydığı
kullanılabilir bellek — root task bunu devredilen Untyped'ların
toplamıyla karşılaştırabiliyor, ve fark kernelin kendine ayırdığı
şeye eşit olmalı. Muhasebe root task'ın *kendi* yapabileceği bir şey
oluyor, kernelin ona söylediği bir sayı değil.

### Neden IPC ile değil

seL4 BootInfo'yu bir sayfada verir; alternatif onu ilk IPC mesajı
olarak göndermek olurdu. Reddedildi iki sebeple: mesaj 128 kelime
(ADR 034) ve Untyped listesi bundan uzun olabilir; ve root task
başlarken karşısında *kimse yok* — mesajı kim gönderecek? Kernel bir
Endpoint'e mi yazacak? O zaman kernelin bir IPC istemcisi olması
gerekir, ki yoktur ve olmamalı.

## Karar 3: root task seri porta `Echo` ile raporluyor

Kabul testi root task'ın BootInfo'sunu raporlamasını istiyor, ve
**kalıcı bir debug syscall'ı eklemeyi yasaklıyor**. Haklı olarak:
bir `Debug.Print` invocation'ı kerneli bir konsol sunucusu yapar ve
bir daha geri alınamaz.

M8'den beri var olan `Echo` kullanılıyor. `Echo` bir capability
adlandırmıyor, kernele hiçbir şey öğretmiyor, ve argümanından
türetilmiş bir değer döndürüyor — tam olarak "round trip çalışıyor"u
kanıtlamak için var. Test gözlemcisi kernelin içinde: dispatcher'ın
`Echo` kolu test modüllerine sırayla soruyor, ve M12'nin modülü
kendi kanalını tanıyor.

Bu bir *test* mekanizması, bir kernel servisi değil, ve ikisinin
farkı şu: `Echo`'nun kanal numarasını yorumlayan kod
`kernel/src/roottest.rs` içinde, `kernel/src/syscall.rs` içinde
değil. Testler çıkarsa yorum da çıkar.

Gerçek sistemde root task seri porta bir **sürücü bileşeni**
üzerinden yazar, ve o bileşen M13'ün işi.

## Karar 4: root task ölürse kernel durur

ADR 026 bir thread'in ölümünü sıradan bir olay yapıyor: TCB revoke
edilir, bekleyenler uyandırılır, sistem devam eder. Root task için
bu yanlış.

**Seçilen: panic.**

Gerekçe, "root task özeldir" değil — öyle olsaydı keyfi olurdu.
Gerekçe şu: root task öldüğünde sistemde **hiçbir Untyped'ı
adlandıran hiçbir capability kalmıyor**. Bütün kaynaklar ona
devredildi; onun CSpace'i gidince bellek hâlâ orada ama artık
ulaşılamaz. Kernel ayakta kalabilir ve hiçbir şey yapamaz: yeni
bileşen kuramaz (M12'den sonra o kodu yok), var olanlara capability
veremez (verecek bir şeyi yok).

Yani alternatif "sistem devam eder" değil, "sistem hiçbir şey
yapmadan devam eder". Bir kilitlenmeyi sessiz bir boşluk olarak
sunmak, ADR 023'ün "her durum tanımlı" kuralının lafzına uyup
ruhuna uymamak olurdu.

Panic mesajı bunu söylüyor, ve `root-dies` senaryosu mesajı
doğruluyor. Exit 35.

Bir gün bir **recovery bileşeni** olursa — kendi Untyped'ını tutan,
root task'ı yeniden kuran bir şey — bu karar değişir. O gün
geldiğinde değişmesi gereken tek yer bu panic.

## Karar 5: kernel boot'tan sonra bileşen kurmuyor

M12'nin asıl iddiası bu, ve test edilebilir olması gerekiyor.

Boot'un sonunda bir bayrak düşüyor: `boot::seal()`. Ondan sonra
`cap::boot`'un kaynak devreden fonksiyonları **panic ediyor**, bir
hata döndürmüyor. Hata döndürmek bir politika olurdu ("çağıran
başa çıksın"); panic bir iddia: bu çağrı olmamalıydı.

Mühürlendikten sonra kernelin yaptığı iki şey kalıyor: **syscall
servisi** ve **kesme teslimi**. Her ikisi de bir bileşenin isteğiyle
başlıyor, hiçbiri kendi başına bir bileşen kurmuyor.

Senaryo 4 bunu bir sayaçla kanıtlıyor: `boot::privileged_ops()`
mühürden sonra sabit kalıyor. Sayaç mühürden *önce* de artıyor —
sıfır olsaydı testin "mühür çalışıyor" ile "o kod hiç yoktu"yu ayırt
etmesi mümkün olmazdı (ADR 023, K5).

## Karar 6: IOAPIC — M10'un borcu

ADR 042 maskelemenin bir yazılım bayrağı olduğunu açıkça yazdı ve
M12'de gerçek IOAPIC'e ineceğini söyledi. Şimdi iniyor.

Değişen: `irq::mask`/`unmask` artık IOAPIC'in redirection table'ına
yazıyor. Değişmeyen: **sözleşme**. "Ack'ten önce ikinci teslimat
yok" ifadesi aynı, `irq-test`'in 4. senaryosu aynı satırları
bekliyor, ve testler değişmiyor.

ADR 042 bunu önceden söylemişti: *"bayrağın zaten sağladığı şey
sözleşme, ve senaryo 4'ün test ettiği o — mekanizma değişince test
değişmiyor."* Bu doğrulanabilir bir iddiaydı ve doğrulanıyor:
`irq-test` tek satır değişmeden geçiyor.

Yazılım bayrağı **kalıyor**, ikinci bir kayıt olarak değil, IOAPIC'in
durumunun aynası olarak — `is_masked` onu okuyor, çünkü IOAPIC'ten
okumak iki MMIO erişimi ve kesme yolunda. Bayrak ile donanımın
ayrışması mümkün mü? Hayır: ikisini yazan tek bir fonksiyon var ve
donanıma önce yazıyor.

## Elenen alternatifler

**Root task'ı kernel thread'i olarak başlatıp sonra ring 3'e
indirmek.** "Daha basit" görünüyor ve değil: iki moda da ait bir
başlangıç durumu, ve "hangi noktadan sonra kullanıcı kodu" sorusu
belirsiz. Root task ilk komutundan itibaren CPL 3.

**BootInfo'yu root task'ın yığınına koymak.** Yığın yazılabilir,
BootInfo olmamalı. Ayrıca yığın büyüyünce üzerine yazılır.

**Kerneli mühürlemek yerine test kodunu `#[cfg]` ile çıkarmak.**
O zaman test edilen ikili gönderilen ikili olmaz — ADR 002'nin
reddettiği şey. Mühür çalışma zamanında, ve testler mührün
*çalıştığını* test ediyor.

## Sonuç

- Root task ayrı bir crate, ham ikili olarak kernelin `.rodata`'sına
  gömülü. Tek dosya, derleme zamanı bağımlılık, ayrıştırıcı yok.
- **BootInfo bir ABI**: `0x0060_0000`'da salt okunur bir sayfa,
  magic ve version ilk iki alan, `header_size` ile ileriye dönük.
- Raporlama `Echo` ile; kalıcı bir debug syscall'ı yok. Yorumlayan
  kod test modülünde.
- **Root task ölürse kernel panic ediyor**, çünkü ondan sonra
  sistemde ulaşılabilir hiçbir kaynak kalmıyor — "devam eder" bir
  boşluğu sessizce sunmak olurdu.
- Boot'un sonunda `seal()`: kaynak devri fonksiyonları artık panic
  ediyor. Bir sayaç mührün önce ve sonrasını ayırt ediyor.
- IOAPIC gerçek maskelemeye indi. **Sözleşme değişmedi** ve bunun
  kanıtı `irq-test`'in tek satır değişmeden geçmesi.

---

## Uygulama bulguları

Aşağıdakiler M12 yazılırken **ortaya çıkan** şeyler. Planlanmadılar;
kod yazılırken veya senaryolar koşturulurken bulundular, ve birkaçı
yukarıdaki kararlardan daha önemli çıktı.

### 1. `mint()` capability'leri bozuyordu

`mint` rozeti `extra` alanına yazıyor. O alan bir CNode'da **radix**,
bir Untyped'da **size_bits** tutuyor (ADR 037). Yani bir CNode'u
mint etmek, "64 slot" bilgisini rozetle değiştiriyordu — ve o
CNode'a yapılan her indeksleme aralık dışı sayılırdı.

Mevcut koruma `src.extra() != 0 && badge != src.extra()` idi ve
**sadece tesadüfen tutuyordu**: bir CNode'un radix'i ve bir
Untyped'ın size_bits'i *genellikle* sıfır değil. Radix 0 olan bir
CNode veya size_bits 0 olan bir Untyped doğrudan geçerdi; rozeti
radix'e eşit olan bir mint de öyle.

Yaygın değerler için tutan, gerisi için tutmayan bir kontrol, önemli
olduğu güne kadar doğru olanından ayırt edilemez (ADR 023, K5). Soru
aslında hep bir **tip** sorusuydu: rozet, rozet taşıyan iki tipe
aittir. Artık tip kapısıyla.

### 2. `Copy` invocation'ı hiç yoktu

M12 `Mint`'i "bir capability'yi devretmenin yolu" diye ekledi ve
`Method::Mint`'in doc yorumu şöyle diyordu: *"sıfır rozet rozetsiz
bir kopyadır, yani dar işlem geniş olanın içinden erişilebilir."*

Bu cümle iki kez yanlış. Mint rozeti **her durumda** yazar, yani
sıfır bir no-op değil bir *silme*; ve `mint` `GRANT` ister, düz
kopyalama istemez.

Ortaya çıkışı: root task bir çocuğa CSpace'ini vermeye çalıştı. Bir
CSpace bir CNode'dur, rozeti olamaz — ve devretmenin *temel* biçimi
rozetsiz olanıdır. Özel olan rozet takandır.

### 3. "Hiçbir çerçeve kaybolmuyor" inşa gereği doğru değil

Kriterin bariz uygulaması `kernel = usable - untyped` bildirip root
task'a `untyped + kernel == usable` kontrolü yaptırmak. Bu özdeşlik
**inşa gereği doğru**: başarısız olamaz, dolayısıyla hiçbir şey test
etmez.

Dahası, gerçek cevap sıfır değil. `cap::boot` belleği `2^k`
sınırında `2^k` baytlık Untyped'lara böler (ADR 020); böyle bir
bloğa sığmayan bir boş çerçeve hiçbir Untyped'a girmez, ve
`retire()` sonrası ona ulaşmanın başka yolu yoktur. Sızdırılmış
değil — *isimlendirilemez*, ki bu daha kötü: hiçbir revoke onu geri
getirmez.

O yüzden `lost_bytes` dördüncü bir terim olarak **raporlanıyor**,
yutulmuyor. Kendi artığını soğurarak denkleşen bir toplam hiçbir şey
kanıtlamaz.

Üç rakam üç ayrı mekanizmadan geliyor — capability'lerden, çağrı
sayımından, bitmap'ten — böylece birbirleriyle çelişebiliyorlar.

### 4. Sayfa tablosu paylaşımı seviye başınadır

Kod 4 MiB'de, yığın 5'te, BootInfo 6'da. İlk sürüm her iki adres
için aynı üç tablo capability'sini sundu ve baştan sona
`AlreadyMapped`'i kabul etti — gereksiz sunumların zararsızca
reddedileceği gerekçesiyle.

Üçünün ikisi gerçekten gereksiz. Üçüncüsü değil: bir PT 2 MiB
kapsar, yani 4..6 MiB bir tablo, 6..8 MiB başkası. 6 MiB'nin var
olmayan bir PT'ye ihtiyacı vardı ve sunulan capability zaten 4
MiB'de haritalıydı.

Hata bir adım sonra, `map_frame`'den `AlreadyMapped { address:
6291456 }` olarak yüzeye çıktı — eksik tabloyu değil, onun üstündeki
BootInfo haritalamasını işaret ederek. **Bir hatayı toptan kabul
etmek, kesin bir başarısızlığı yanıltıcı bir başarısızlığa çevirdi.**

Bu M11'in `SCRATCH_ADDRESS` dersinin öbür yönden gelişi. Orada
"PDPT ve PD'yi paylaşıyorlar" doğru, güven verici ve PT hakkında
sessizdi. Burada aynı yarım gerçek bir döngüye yazılmıştı.

### 5. Makefile prerequisite'leri parse anında genişler

`$(KERNEL)`'e `$(ROOTTASK_BIN)` bağımlılığı eklendi ve **sessizce
hiçbir şey yapmadı**. Kural ~200. satırda; değişken 570 satır sonra
tanımlı. Parse anında boş genişledi: hata yok, uyarı yok, kaynak
imajdan yeniyken bile `make -n` hiçbir yeniden derleme göstermiyor.

Belirti, iki düzenleme önceki bir root task binary'sine karşı
başarısız olan bir senaryoydu.

Ekleme yapılarak büyüyen bir Makefile için genel ders: bir **kural**
eklemek güvenli, daha sonra tanımlanan bir değişkeni adlandırmak
için önceki bir kuralı düzenlemek değil. Make aynı hedef için
prerequisite'leri birleştirir, o yüzden doğru düzeltme de ekleme.

### 6. Dönen senaryolar capability sistemini dağıtıyor

Root task'ı `kmain`'in sonunda koşulsuz başlatmak, yazılmamış bir
değişmezi kırdı.

Senaryoların çoğu QEMU'dan çıkar ve dönmez. `cap-test` döner — ki
hiçbir şey ardından gelmezken zararsızdı. Burada zararsız olmaktan
çıktı: `captest` `clear_objects()` çağırıyor, bu da nesne CNode'unu
boşaltıyor (254'teki IRQControl dahil), ve root task bir testin
söktüğü capability durumunun üstünde başlıyordu.

Başarısızlık `cap-test`'e karşı raporlandı — ki kendisi geçmişti.

### 7. root-dies'ta kerneli *kernel* fark etmiyordu

İlk sürüm TCB'yi siliyor, bekliyor, sonra kendi `unreachable!`'ıyla
başarısızlık ilan ediyordu. Tam da olan buydu: zamanlayıcı o
thread'i zamanlamayı bıraktı ve boot thread'iyle gayet memnun devam
etti. Panic **testten** geliyordu.

Ki bu değersiz olurdu. Hatayı kendi tespit eden bir negatif senaryo,
senaryonun çalıştığını kanıtlar, kernelin değil — sistem kimsenin
bakmadığı bir makinede sonsuza dek orada otururdu ve ADR 044'ün "bu
bir panic olmalı" cümlesi bir belgedeki cümle olarak kalırdı.

`check_alive()` artık timer tick'inde: adres + generation, ADR 042
ve 036'nın aynı deyimi.

### 8. Çıplak bir `ERROR_FLAG` "hata 0" demektir

`root-no-untyped`'ta root task `abi::ERROR_FLAG`'i tek başına
raporluyordu — bayrak set, kod *sıfır*. Test sadece bayrağa bakıyor
ve geçiyordu; log "reported error 0" yazıyordu.

Yine kendi yokluğundan ayırt edilemeyen bir kontrol: bayrağı
herhangi bir sebeple — veya sebepsiz — set eden bir root task aynı
şekilde geçerdi. Artık `NotEnoughMemory` (11), hem kernelde hem
`run.py`'de tam kodla.

### 9. Doğrulanamayan bir iddia: `syscalls > ops_at_seal`

Kriter 4'e "root task kernelden fazlasını yaptı" diye bir destek
kontrolü eklendi ve başarısız oldu: 103 işleme karşı 61 syscall.

Kernel hiçbir şey büyütmemişti. 103'ün 97'si kök Untyped başına bir
`Copy`, yani sayaç **makinenin ne kadar RAM'i olduğuyla** ölçekleniyor.
64 MiB'lik bir makinede geçerdi; 1 GiB'likte daha sert düşerdi.
Karşılaştırılan nicelik, iddianın konusu olan nicelik hiç değildi.

Ayarlanacak dürüst bir eşik yok, çünkü iki sayı ilgisiz şeyleri
ölçüyor. Kriter 4'ün gerçek içeriği eşitlik — mühürden sonra sayaç
kıpırdamıyor — ve onu aritmetik değil, `note_privileged_op`'un panic
etmesi zorluyor. Destek kılığına girmiş ikinci, zayıf bir kontrol
hiç olmamasından kötü olurdu: iddiayla ilgisiz sebeplerle
başarısız olur ve bir sabit yükseltilerek "düzeltilirdi".

### 10. IOAPIC: ne satın aldığı, ne almadığı

Maskeleme donanıma indi ve ADR 042'nin sözleşmesi değişmedi — ama
dürüst anlatım "maske artık gerçek"ten daha dar.

**Yazılım bayrağı kalıyor ve sözleşmeyi hâlâ o zorluyor.** Kesme
yolu onu tek atomik yükle okuyor; select/data penceresinden bir
aygıt register'ı okumak, eşzamanlı bir yazmaya karşı atomik
yapılamayacak iki önbelleksiz erişim olurdu.

Donanımın eklediği şey, hattı **kaynağında** durdurmak: maskeli bir
hat artık dispatch ve EOI maliyeti getirmiyor. Bu gerçek bir aygıt
için önemli, `irq-test` için görünmez — o senaryo kesmelerini
self-IPI ile üretiyor ve self-IPI LAPIC'ten CPU'ya gider, IOAPIC'e
hiç uğramaz.

Yani donanım maskesinin `irq-test`'i geçiren şey olduğunu iddia
etmek doğrulanamaz olurdu: çip takılı olmasa da aynı geçerdi.
Doğrulanabilir olan şu: register ne yazıldıysa onu tutuyor.
`ioapic::verify` her yönlendirme girdisini çipten geri okuyup
yazılım bayrağıyla karşılaştırıyor — M13'ün test edilmemiş bir yol
devralmaması için.

### K5 mutasyonu: iki bağımsız öldürme

Zorunlu mutasyon — BootInfo'da bir eksik Untyped bildir — **iki kez**
yakalanıyor:

1. sayı kontrolü: `BootInfo 96 diyor, kernel 97 yarattı`;
2. o kontrol devre dışı bırakılınca **muhasebe**, tam 4096 bayt
   eksikle: `199811072 + 67473408 + 0 != 267288576`.

İkincisi, senaryo 3'ün dekoratif olmadığını gösterdiği için
yapıldı. Tek bir öldürme, muhasebenin de yakalayıp yakalamayacağını
söylemezdi.

### 11. Yük altında kırılgan bir senaryo

Son doğrulamada `vspace-user-ro` bir kez başarısız oldu, sonra iki
kez üst üste geçti. Tek başına koşturulduğunda doğru hatayı tam
olarak üretiyor:

```
[trap] cr2=0x0000000000700000  (user write protection-violation)
```

Yani senaryo doğru; başarısızlık **zamanlama**. O senaryo hatayı
sabit bir TSC penceresinde bekliyor, ve TSC döngüleri yüklü bir
konakta duvar saati değil — birkaç QEMU örneği aynı anda koşarken
pencere kapanabiliyor.

Burada düzeltilmedi, çünkü M12'nin dokunmaması gereken bir dosyada
(ADR 023, K8'in kendi alanı) ve asıl soru pencerenin ne kadar
olacağı değil, *neden bir pencere olduğu*. Kaydediliyor: M13'te
gerçek bir sürücü varken bu tür beklemeler artacak, ve "bazen
geçen" bir test hiç olmayandan kötüdür.