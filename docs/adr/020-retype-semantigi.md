# ADR 020 — Retype: atomiklik, hizalama, ve belleğin geri dönüşü

Status: accepted (M5 architect pass)

Kaynak: seL4 Reference Manual bölüm 2.4 ("Kernel Memory Allocation") ve
4.1 ("Untyped Memory").

## Bağlam

`retype` capability modelinin tek bellek ayırma yolu. Bir Untyped cap'i
alır, içinden N adet nesne üretir, cap'lerini verilen CNode slot'larına
yazar.

```
retype(untyped, object_type, size_bits, count, dest_cnode, dest_offset)
```

Dört soru cevaplanmalı:

1. **Atomiklik.** Yarıda kesilirse ne olur?
2. **Hizalama ve yerleşim.** Nesneler Untyped içinde nereye düşer?
3. **Revoke ile ilişki.** Untyped revoke edilince nesneler ne olur?
4. **Bilgi sızıntısı.** Bellek yeniden kullanıma dönerken temizleniyor
   mu?

## 1. Atomiklik: ya hep ya hiç, ve kesilemez

Retype **ya tamamen başarılı olur ya hiç bir şey yapmaz.** Yarım bir
sonuç yok: 10 nesne istenip 7'si yaratılmış bir durum oluşamaz.

Nasıl: **önce tüm kontroller, sonra tüm yazmalar.**

```
retype:
    faz 1 — doğrula, hiçbir şeye dokunma:
        untyped cap geçerli mi, tipi doğru mu
        object_type ve size_bits geçerli bir kombinasyon mu
        count * object_size, untyped'ın kalanına sığıyor mu
        dest_cnode'da count tane ardışık BOŞ slot var mı
        her yeni cap'in derivation derinliği <= 8 olacak mı (ADR 019)

    faz 2 — yaz, hiçbiri başarısız olamaz:
        belleği sıfırla
        her nesneyi başlat
        cap'leri slot'lara yaz, derivation listesine ekle
        untyped'ın watermark'ını ilerlet
```

Faz 2'nin başarısız olamamasının sebebi: faz 1 her kaynağı önceden
kontrol etti ve **retype sırasında başka hiçbir şey çalışmıyor** —
[ADR 019](019-derivation-tree.md)'un revoke için koyduğu kural burada da
geçerli: syscall içinde thread değişimi yok.

Kesmeler? Retype'ın faz 2'si en fazla `count` nesne başlatır ve
`count` slot yazar. Revoke'tan farklı olarak bu **baştan bilinen bir
sayı**, ve çağıran onu seçiyor. Yine de bir üst sınır koyuyoruz:

**`count <= 256`.** Daha fazlası birden çok çağrı gerektirir. Gerekçe:
tek bir syscall'ın en kötü durumu sınırlı kalsın. 256 × 4 KiB = 1 MiB
bellek, tek seferde başlatılması makul bir miktar.

## 2. Hizalama ve yerleşim: watermark, geri alma yok

Bir Untyped'ın içinde bir **watermark** var: "buraya kadar dağıtıldı".
Retype, watermark'ı nesne hizalamasına yuvarlar, oradan `count` nesne
keser, watermark'ı ilerletir.

```
Untyped: 2^20 bayt (1 MiB), base 0x200000

  0x200000          0x201000      0x201400         0x300000
  |-----------------|-------------|----------------|
  | Frame (4 KiB)   | TCB (1 KiB) | henüz dağıtılmamış
                                  ^
                                  watermark
```

**Watermark asla geri gitmez** — nesneler tek tek silinse bile. Bir
Untyped'ın belleği ancak **revoke** ile geri gelir, ve o zaman watermark
sıfırlanır.

Bu, seL4 ile aynı ve ilk bakışta israf gibi görünüyor: ortadaki bir
nesneyi silip yerine başkasını koyamıyorsunuz. Sebebi, alternatifin
maliyeti: bir Untyped içinde serbest listeler tutmak, Untyped'ı bir
allocator'a çevirir. O zaman retype'ın maliyeti "uygun boşluk ara"
olurdu — [ADR 016](016-kernel-heap-allocator.md)'da heap için elediğimiz
sınırsız arama, burada tekrar.

Watermark modeli, **bellek yönetimi politikasını user space'e itiyor**:
bir bileşen belleği geri kazanmak istiyorsa Untyped'ları küçük tutar ve
tamamını revoke eder. Bu bir kısıt değil, modelin özü — kernel politika
yapmıyor.

### Hizalama kuralı

Nesne hizalaması [ADR 017](017-nesne-kumesi.md)'de: boyutun bir sonraki
ikinin kuvveti. Watermark o değere yuvarlanır:

```
start = (watermark + align - 1) & !(align - 1)
```

Hizalama 2'nin kuvveti olduğu için tek bir maskeleme. Yuvarlamadan
doğan boşluk kaybolur (watermark geri gitmediği için) — 1 KiB hizalı bir
TCB için en fazla 1023 bayt. Kabul.

**Untyped'ın kendisi de hizalı:** `2^k` boyutunda ve `2^k` hizalı. Bu,
bir Untyped'ın ikiye bölünmesini (iki alt Untyped'a retype) tek bir
kaydırma yapıyor, ve bir adresin hangi Untyped'a ait olduğunu bulmayı
maske işlemine indiriyor.

## 3. Revoke edilen Untyped: nesneler önce ölür

`revoke(untyped)` çağrıldığında, [ADR 019](019-derivation-tree.md)'un
listesi gezilir ve o Untyped'dan türeyen **her cap** silinir. Her cap
silindiğinde, o nesnenin son cap'i miydi diye bakılır; öyleyse nesne
yok edilir.

Nesne yok etmek nesneye göre değişiyor ve bu kısım kritik:

| Nesne | Yok edilirken |
|---|---|
| `Frame` | Eşli olduğu **her** VSpace'ten kaldırılmalı |
| `PageTable` | Üst tablodan kaldırılmalı, altındakiler zaten ayrı cap |
| `TCB` | Thread durdurulmalı, kuyruklardan çıkarılmalı |
| `Endpoint` | Bekleyen thread'ler uyandırılmalı (hata ile) |
| `Notification` | Bekleyenler uyandırılmalı |
| `CNode` | İçindeki cap'ler zaten derivation listesinde, ayrıca silinir |
| `IRQHandler` | IRQ maskelenebilmeli |
| `Untyped` | Çocukları zaten listede |

**Frame satırı bir problem.** Bir Frame birden çok VSpace'e eşlenmiş
olabilir ve her eşlemeyi bulmak, tüm sayfa tablolarını taramak demektir.
seL4 bunu Frame başına bir eşleme listesi tutarak çözer.

Biz **M11'e kadar bir Frame'in en fazla bir kez eşlenmesine izin
vereceğiz.** Frame nesnesi tek bir `(vspace, vaddr)` kaydı tutar; ikinci
`map` denemesi `AlreadyMapped` döner. Paylaşımlı bellek isteyen iki
bileşen, aynı Untyped'dan **iki ayrı Frame cap'i** alır — ki bu aynı
fiziksel belleğe iki cap demek değil, iki ayrı 4 KiB demek.

Yani **M11'e kadar paylaşımlı bellek yok.** Açıkça kabul ediyorum.
Gerçek paylaşım M11'de Frame'e bir eşleme listesi eklemeyi gerektirecek
ve bu, Frame nesnesinin boyutunu değiştirecek.

> **M9 için açık ön koşul.** IPC tasarımı, paylaşımlı belleğin
> olmadığını **varsayarak** yazılacak. Büyük veri transferi için
> "iki bileşen aynı Frame'i eşlesin" seçeneği yok; her transfer ya
> kayıtlarda ya TCB'nin 1 KiB'lık IPC tamponunda
> ([ADR 017](017-nesne-kumesi.md)) taşınacak, ya da veri bir Frame
> cap'inin **devredilmesiyle** (kopyalanmasıyla değil) geçecek.
>
> Bu, M13'ün hedefiyle tutarlı: init sistemi dosya sistemi ya da ağ
> yığını içermeyecek — birkaç küçük servis ve bir seri sürücü. 1 KiB
> mesaj yeterli.
>
> M9'u yazarken bu kısıt bir yere sıkıştırırsa, çözüm IPC'yi
> karmaşıklaştırmak değil, Frame'e eşleme listesi eklemeyi M11'den
> M9'a çekmek olmalı.

## 4. Bilgi sızıntısı: sıfırlama ne zaman

**Retype, belleği nesneyi başlatmadan önce sıfırlar.** Her zaman, istisnasız.

Alternatif (yok ederken sıfırlamak) daha verimli görünür — bellek zaten
çöpe gidiyor. İki sebeple reddedildi:

1. **Yok etme yolu daha fazla:** nesne silinerek, Untyped revoke
   edilerek, ya da bileşen çökerek. Her birinde sıfırlamayı hatırlamak
   gerekir; biri unutulursa sızıntı sessiz.
2. **Retype tek giriş.** Bellek user'a *ancak* retype ile ulaşır. Tek
   bir yerde sıfırlamak, "sızıntı imkânsız" demenin en ucuz yolu.

Maliyet: 4 KiB Frame için bir `rep stosq`, ~512 döngü. `count <= 256`
sınırıyla en kötü 1 MiB sıfırlama, ~130k döngü. Tek bir syscall için
yüksek ama sınırlı, ve güvenlik için ödenen fiyat açık.

**İstisna yok.** "Bu nesne zaten sıfırdı" optimizasyonu bile yok: onu
bilmek, bir bit daha tutmak ve o bitin doğruluğuna güvenmek demek.

### Kernel nesneleri user'a asla görünmüyor

Bir TCB ya da Endpoint, user'ın eşleyebileceği bir şey değil — yalnızca
Frame eşlenebilir. Yani bir kernel nesnesinin içeriği, nesne yok
edildikten sonra bile user'a doğrudan sızamaz. Sızabileceği tek yol,
o belleğin **Frame'e retype edilip** eşlenmesi — ki o yolda sıfırlama
var.

## Hata durumları

```
InvalidUntyped            cap Untyped değil
InvalidObjectType         bilinmeyen tip
InvalidSize               size_bits bu tip için geçersiz
                          (sabit boyutlu tipler için size_bits=0 olmalı)
NotEnoughMemory           count nesne untyped'ın kalanına sığmıyor
                          (kalan bayt sayısı hata ile birlikte döner)
SlotsNotEmpty             hedef slot'lardan biri dolu
SlotsOutOfRange           dest_offset + count, CNode'un dışına taşıyor
DerivationTooDeep         sonuç derinlik > 8 (ADR 019)
TooManyObjects            count > 256
DeviceUntypedFrameOnly    device Untyped'dan Frame olmayan bir tip
                          isteniyor (ADR 017)
```

Hepsi faz 1'de yakalanıyor, yani hata dönen bir retype hiçbir şeye
dokunmamış oluyor.

`NotEnoughMemory`'nin kalan baytı döndürmesi bilinçli: çağıran "ne kadar
sığar" diye ayrıca sormak zorunda kalmasın.

## Elenen alternatifler

**Kısmi başarı (N istendi, M yapıldı).** Çağıranın hata yolunu
karmaşıklaştırır ve "hangi slot'lar doldu" sorusunu doğurur. Ya hep ya
hiç, akıl yürütmesi kolay olan.

**Watermark yerine serbest liste.** Untyped'ı allocator'a çevirir; retype
sınırsız arama yapar. Reddedildi, gerekçe yukarıda.

**Sıfırlamayı atlamak, "performans".** Bilgi sızıntısı. Bir mikroçekirdek
için kabul edilemez — izolasyon tek satış argümanı.

**Sıfırlamayı yok etme anında yapmak.** Birden çok yok etme yolu, her
birinde hatırlanması gereken bir adım. Retype tek giriş noktası.

**`count` sınırı koymamak.** Tek bir syscall'da 65536 nesne
başlatılabilirdi. 256, en kötü durumu 1 MiB sıfırlama + 256 nesne
başlatma ile sınırlıyor.

**Untyped'ı bölmeden doğrudan nesne üretmek (bölme yok).** Bir
Untyped'dan hem Frame hem TCB üretilebiliyor ve bu iyi; alt-Untyped'lar
ise **bütçe devretmek** için gerekli — bir bileşen diğerine "şu kadar
bellek kullanabilirsin" diyebilmeli. İkisi de var.

## Sonuç

- Retype atomik: faz 1 doğrular, faz 2 yazar ve başarısız olamaz.
- `count <= 256`; tek syscall'ın en kötü durumu sınırlı.
- Watermark modeli, geri alma yok. Bellek yalnızca revoke ile döner.
  Bellek yönetimi politikası user space'te.
- Untyped `2^k` boyutlu ve `2^k` hizalı; nesne hizalaması tek maskeleme.
- **Bellek her zaman retype anında sıfırlanıyor.** İstisnasız, çünkü
  retype tek giriş.
- Revoke, nesneleri tipine göre söküyor; Frame'in eşlemesi en zor
  kısım.
- **M11'e kadar bir Frame en fazla bir yere eşlenebilir — yani
  paylaşımlı bellek yok.** M9'un IPC tasarımı için açık bir ön koşul
  olarak yukarıda yazılı.
- **Device Untyped yalnızca Frame'e retype edilebilir**
  ([ADR 017](017-nesne-kumesi.md)). Faz 1'de denetleniyor,
  `DeviceUntypedFrameOnly`.
- Dokuz hata türü, hepsi faz 1'de.

---

## M14 eki — sıfırlama kuralı RAM'e özgüdür

Yukarıdaki 4. bölüm şunu yazıyor:

> **Retype, belleği nesneyi başlatmadan önce sıfırlar.** Her zaman,
> istisnasız.
>
> **İstisna yok.**

**Bu cümle RAM için doğru, device bellek için yanlıştı.** M14'te ilk
gerçek device Untyped ortaya çıktığında kernel bir `#PF` ile durdu:
`memset`, `pmap_base + 0xfd000000` adresine yazmaya çalıştı.

Kural değişmiyor; **kapsamı yazılıyor.** Device bellek muaf, ve
gerekçe iki ayrı sebep — ikisi de tek başına yeterli.

### (i) İmkânsız

Fiziksel pencere yalnızca RAM'i kapsıyor
([ADR 027](027-vspace-modeli.md), değişmez 2) ve `vm::init`
framebuffer bölgelerini zaten dışarıda bırakıyor. `phys_to_virt` o
adres için eşlenmemiş bir sanal adres üretir; yazma sayfa hatası
verir.

Bu, "dikkatli olalım" değil, aritmetik: sıfırlamak için bir eşleme
gerekir, ve o eşleme bilerek yok.

### (ii) Yanlış

Eşlenseydi bile yapılmamalıydı. Bir framebuffer'ı sıfırlamak ekranı
siler. Bir aygıtın register penceresini sıfırlamak, o register'lara
yazmaktır — yani `retype`, bir muhasebe işleminin yan etkisi olarak
**donanım komutu verir**. Framebuffer'da bu görsel bir bozulma; bir
disk denetleyicisinde bir reset.

ADR 017'nin device Untyped'ı icat etme gerekçesi tam olarak buydu:
aygıt belleğine kernel'in kendi amaçları için yazmak, aygıta komut
vermekle aynı şey.

### Sızıntı argümanı neden taşınmıyor

4. bölümün savı şu: bir nesnenin baytları, o belleğin **bir sonraki
sahibine** ulaşmamalı. Argüman bir *geçmişle* ilgili — önceki
sahibin kalıntısı.

Framebuffer'ın içeriği önceki bir sahibin kalıntısı değil. **Cihazın
şu anki durumu.** Ekranda ne varsa odur, ve ekran zaten paylaşılan
bir şey: yanındaki kişi de görüyor. Yeni sahibin okuyabileceği hiçbir
şey, ekrana bakarak zaten öğrenemeyeceği bir şey değil.

Dolayısıyla sıfırlama burada bir güvenlik önlemi değil, hiçbir şeyi
korumayan bir yazma olurdu — ve (ii) gereği zararlı bir yazma.

### Kodda

`untyped.rs`'nin 2. fazında tek bir koşul:

```rust
if !untyped.is_device() {
    // ... write_bytes(start, 0, ...)
}
```

Dal, adlandırılmış bir karar olarak duruyor: yorumu bu paragrafa
atıf veriyor, yoksa bir okuyucu onu bir optimizasyon sanabilir —
ki 4. bölümün açıkça reddettiği şey o.

### Bu kuralın üçüncü kırılması

Aynı varsayımdan — *"bir Untyped'ın belleği yazılabilir RAM'dir"* —
M14'te üç kural birden kırıldı:

| # | Kural | Nerede yaşıyordu | Device için |
|---|---|---|---|
| 1 | Untyped kendi başlığını taşır | ilk 16 bayt | başlık yok |
| 2 | watermark başlıkta | ilk 8 bayt | kernel tablosunda ([ADR 050](050-boot-device-untyped-tohumlama.md)) |
| 3 | retype her zaman sıfırlar | bu ADR | muaf, yukarıdaki iki sebeple |

Üçü de M5'te yazıldı, dokuz milestone boyunca doğru kaldı, ve ilk
gerçek aygıt belirdiğinde aynı anda kırıldı. [ADR 023](023-test-disiplini.md)'ün
K11'i bu gözlemden doğdu.

### Tarandı: dördüncüsü var mıydı

`retype` / `copy` / `revoke` / `delete` yolları device gözüyle bir kez
daha tarandı. Dördüncü bir *kural* kırılması bulunmadı, ama bir **kod**
hatası bulundu: `boot::root_untyped_with_room` device belleği
elemiyordu — dört kardeşi (`roottask`, `usertest`, `vspacetest`,
`vspaceuser`) eliyordu, bu en eskisi olduğu için eklenmemişti.
Düzeltildi.

Negatif sonuç da sonuçtur: `copy`, `revoke` ve `delete` device
Untyped'ın belleğine hiç dokunmuyor — ilki yalnızca slot yazıyor,
diğer ikisi yalnızca slot temizliyor. Nesne *yok etme* device
Untyped'a ulaşmıyor çünkü ondan yalnızca Frame çıkabiliyor ve bir
Frame'in yok edilmesi bir eşlemeyi kaldırmaktan ibaret.