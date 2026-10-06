# ADR 018 — CPtr formatı: iki seviye, sabit derinlik, guard yok

Status: accepted (M5 architect pass)

Kaynak: seL4 Reference Manual, bölüm 3 ("Capability Spaces"). Bu, seL4'ün
en zor anlaşılan parçası ve ondan en çok ayrıldığımız yer.

## Bağlam

Bir bileşen "şu Endpoint'e mesaj gönder" dediğinde, hangi Endpoint'i
kastettiğini nasıl söyler? Cevap: bir **capability pointer** (CPtr) ile —
kendi CSpace'indeki bir slot'un adresi.

CSpace, CNode'lardan oluşan bir ağaç: bir CNode slot'u başka bir CNode'u
gösterebilir. CPtr bu ağaçta bir yol tarif eder. Sorular:

1. Ağaç ne kadar derin olabilir?
2. Bir CPtr'ın bitleri nasıl bölünür — hangi bitler hangi seviyeye?
3. Çözümleme (resolution) ne kadar sürer, ve en kötü durumu nedir?

Üçüncüsü kritik: CPtr çözümleme **her syscall'ın ilk işi**. M9'da bir IPC
çağrısı en az iki CPtr çözecek (hedef Endpoint, belki taşınan cap).
Maliyeti sabit ve küçük olmalı.

## seL4'ün yaptığı: guard'lı radix ağacı

seL4'te her CNode'un iki parametresi var: **radix** (kaç bitlik indeks,
yani 2^radix slot) ve **guard** (bu CNode'a girmeden önce CPtr'dan
tüketilecek sabit bir bit deseni).

Çözümleme şöyle: CPtr'ın en anlamlı bitlerinden başla, guard ile
karşılaştır, uyuşmazsa hata; uyuşursa guard kadar bit tüket, sonra radix
kadar bit al, o slot'a bak. Slot bir CNode ise aynısını tekrarla.

Guard'ın amacı: **sığ ağaçlarda bit israfını önlemek.** 64 bitlik bir
CPtr'ın tamamını tüketmek zorunludur (seL4'te çözümleme "tüm bitler
tükendi" ile biter), ve guard olmadan sığ bir ağaç için CPtr'ın üst
bitlerini sıfırla doldurmak gerekirdi — ki bu çalışır ama her CNode'un
derinliği CPtr'ın yapısına gömülür.

Bedeli: bir CPtr'ın ne anlama geldiği, **hangi CSpace'te çözüldüğüne
bağlı**. Aynı 64-bit sayı iki bileşende iki farklı nesneyi gösterebilir —
ki bu doğru ve istenen bir özellik — ama aynı zamanda ağacın şekli
değişince aynı CPtr başka bir şeye çözülür. Hata ayıklamak zor, ve
"yanlış CPtr" ile "yanlış şekilli ağaç" aynı belirtiyi verir.

## Seçim: iki seviye, sabit derinlik, guard yok

```
CPtr: 64 bit

 63            32 31            16 15             0
+----------------+----------------+----------------+
|   sıfır (32)   |   L1 indeks    |   L0 indeks    |
+----------------+----------------+----------------+
                      16 bit           16 bit
```

- **L0 (yaprak CNode):** 2^16 = 65536 slot'a kadar. Pratikte çok daha
  küçük; radix her CNode'da saklı.
- **L1 (kök CNode):** 2^16 alt CNode'a kadar.
- **Üst 32 bit sıfır olmalı.** Değilse `InvalidCPtr`. Boş bırakılmadı,
  *rezerve edildi* — gerekçe aşağıda.

Çözümleme:

```
resolve(cspace_root, cptr):
    if cptr >> 32 != 0:  return InvalidCPtr
    l1_index = (cptr >> 16) & 0xFFFF
    l0_index =  cptr        & 0xFFFF

    if l1_index >= cspace_root.slots:  return OutOfRange
    slot = cspace_root[l1_index]

    if slot.type == CNode:
        if l0_index >= slot.cnode.slots:  return OutOfRange
        return &slot.cnode[l0_index]
    else:
        if l0_index != 0:  return InvalidCPtr   // bkz. "tek seviye"
        return &slot
}
```

**En kötü durum: iki bellek erişimi, üç karşılaştırma, döngü yok.**
Sayıyla: CPtr başına ≤ 20 komut. seL4'ün değişken derinlikli
çözümlemesinde bu, ağacın derinliğine bağlı bir döngü.

### Tek seviyeli kullanım

Bir bileşenin tek bir CNode'u varsa (M13'e kadar beklenen durum),
cap'lerini doğrudan kök CNode'a koyar ve L0 indeksini sıfır bırakır. O
zaman CPtr = `index << 16` olur.

Bu biraz tuhaf görünüyor — neden alt 16 bit değil? Çünkü alternatif,
"slot CNode değilse L1 indeksini kullan" demek olurdu ve o zaman aynı
CPtr'ın hangi yarısının kullanıldığı ağacın şekline bağlı olurdu. Tam
olarak seL4'ün guard'ından kaçındığımız belirsizlik. Böylece kural tek:
**L1 her zaman kökte indeksler, L0 her zaman yaprakta.**

### Üst 32 bit neden rezerve

Sıfır olmak zorunda ve bu zorunluluk denetleniyor. Üç olası gelecek
kullanım:

1. **Üçüncü bir seviye** (bit 32–47), eğer iki seviye yetmezse.
2. **CSpace etiketi** — hangi CSpace'te çözüleceğini belirten bir alan,
   bir bileşenin başka bir bileşenin CSpace'ine kontrollü erişimi için.
3. **Nesil (generation) sayacı** — bir slot yeniden kullanıldığında eski
   CPtr'ların otomatik geçersizleşmesi için.

Üçünü de şimdi yapmıyoruz. Ama alanı sıfır olmaya *zorlamak*, ileride
anlam yüklemeyi mümkün kılıyor: bugün sıfır olmayan bir CPtr hata
veriyorsa, yarın anlam kazandığında eski kod kırılmaz.

## Ne kaybediyoruz

**1. Derin CSpace'ler.** 2^32 slot adreslenebiliyor (65536 CNode × 65536
slot), ama hiyerarşi iki seviyeyle sınırlı. seL4'te bir bileşen keyfi
derinlikte alt-CSpace'ler kurabilir; burada kuramaz.

Bunun gerçek maliyeti: **CSpace'i delege etmek zorlaşıyor.** seL4'te bir
bileşen, alt bileşenine kendi CSpace'inin bir dalını verip "bu senin
CSpace'in" diyebilir ve o alt bileşen kendi içinde yine dallanabilir.
Bizde alt bileşen tek seviye derinliğinde kalır.

M13'e kadar sorun değil: init bileşeni + birkaç sürücü + birkaç
uygulama, her biri kendi düz CNode'uyla. Üç kademeli bir bileşen
hiyerarşisi kuran bir sistem yazmak istersek bu kapı kapalı olacak ve
üçüncü seviyeyi (rezerve bitlerde) açmak gerekecek.

**2. Değişken bit bölüşümü.** seL4'te 10 slot'luk bir CNode 4 bit, 1000
slot'luk bir CNode 10 bit tüketir. Bizde ikisi de 16 bit yerinde durur,
yani küçük CNode'larda CPtr uzayı israf oluyor. 2^32 adreslenebilir
slot'un yanında bu israf önemsiz.

**3. seL4 uygulamalarının doğrudan taşınabilirliği.** CPtr hesaplayan bir
seL4 kodu bizde çalışmaz. Zaten hedef değildi.

## Elenen alternatifler

**seL4'ün guard'lı radix ağacı, aynen.** En esnek, ve doğru çalıştığı
kanıtlanmış. İki nedenle alınmadı: çözümleme bir döngü içeriyor (derinlik
bağımlı, yani en kötü durumu CSpace'in şekline bağlı), ve guard semantiği
hata ayıklamayı zorlaştırıyor. Bu projede birincisi daha ağır bastı —
"kernel yolunda sınırsız döngü yok" kuralı CPtr çözümlemesini de kapsıyor.
*Not:* seL4'te derinlik pratikte CPtr'ın bit sayısıyla sınırlı (en fazla
64 adım), yani teknik olarak sınırsız değil. Ama 64 adımlık bir en kötü
durum ile 2 adımlık bir en kötü durum arasındaki fark, bir IPC yolunda
ölçülebilir.

**Tek seviye, düz dizi.** CPtr doğrudan bir slot indeksi. En basit ve en
hızlı: bir karşılaştırma, bir indeksleme. Reddedildi çünkü CSpace
delegasyonunu *tamamen* imkânsız kılıyor — bir bileşen diğerine bir cap
grubu veremez, yalnızca tek tek cap'ler verebilir. İki seviye, M13'ün
ihtiyacı olan minimum hiyerarşi.

**Değişken derinlik ama guard yok.** Çözümleme "slot CNode olduğu sürece
devam et" olurdu. Döngü yine var, ve derinliğin nerede biteceği CPtr'da
yazmadığı için bir döngü sınırı koymak gerekir — yani sabit derinliğin
dezavantajını alıp avantajını almamak.

**CPtr'ı 32 bit yapmak.** Yarım register, ve 2^32 slot zaten fazlasıyla
yeterli. Reddedildi: syscall argümanları zaten 64 bit register'larda
geliyor, 32 bit kullanmak bir şey kazandırmıyor ve rezerve alanı
kaybettiriyor.

**Nesil sayacını şimdi eklemek.** Bir slot yeniden kullanıldığında eski
CPtr'lar sessizce yeni nesneyi gösterir — bu, capability sistemlerinde
bilinen bir tuzak (ABA problemi). Nesil sayacı çözer. Şimdi eklenmedi
çünkü slot başına ek alan ve her çözümlemede bir karşılaştırma daha
demek; ve M13'e kadar slot geri dönüşümü nadir. Rezerve bitler bu
ihtimali açık tutuyor. **Bu bir risk ve açık sorular listesinde, soru 2.**

## Sonuç

- CPtr 64 bit: 32 bit rezerve (sıfır olmalı, denetleniyor) + 16 bit L1 +
  16 bit L0.
- Sabit iki seviye. Çözümleme **döngüsüz**, en kötü durum iki bellek
  erişimi ve üç karşılaştırma, ≤ 20 komut.
- Guard yok. seL4'ten ayrıldığımız ikinci ve en büyük nokta.
- Tek seviyeli kullanımda L0 = 0; kural ağacın şekline bağlı değil.
- **Kapattığımız kapı:** ikiden derin CSpace hiyerarşisi, yani çok
  kademeli bileşen delegasyonu. Rezerve bitlerle ileride açılabilir.
- **Risk:** slot yeniden kullanımında ABA. Nesil sayacı için yer var,
  uygulanmadı. Soru 2.