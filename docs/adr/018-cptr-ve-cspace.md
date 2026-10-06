# ADR 018 — CPtr: iki seviye *zorunlu*, nesil sayaçlı

Status: accepted (M5 architect pass), **düzeltildi (M5 uygulama)**

Kaynak: seL4 Reference Manual, bölüm 3 ("Capability Spaces").

> **Düzeltme notu.** Bu ADR'nin ilk hâli, kaçınmak için yazıldığı hatayı
> kendisi yapıyordu. Ayrıntı aşağıda, "Düzeltilen hata" başlığında. İlk
> hâl hiç uygulanmadı; aşağıdaki tasarım M5'te yazılan.

## Bağlam

Bir bileşen "şu Endpoint'e mesaj gönder" dediğinde hangi Endpoint'i
kastettiğini bir **capability pointer** (CPtr) ile söyler: kendi
CSpace'indeki bir slot'un adresi.

Üç soru:

1. Ağaç ne kadar derin olabilir?
2. CPtr'ın bitleri seviyelere nasıl bölünür?
3. Çözümlemenin en kötü durumu nedir?

Üçüncüsü kritik: CPtr çözümleme her syscall'ın ilk işi. M9'da bir IPC
çağrısı en az iki CPtr çözecek.

Ve dördüncü bir soru, ilk hâlde sorulmamıştı:

4. **Aynı CPtr her zaman aynı yeri mi gösterir?**

## seL4'ün yaptığı: guard'lı radix ağacı

Her CNode'un iki parametresi var: **radix** (2^radix slot) ve **guard**
(girmeden önce CPtr'dan tüketilen sabit bit deseni). Çözümleme, CPtr'ın
bitlerini guard/radix çiftleriyle tüketerek ilerler ve tüm bitler
tükenince biter.

Guard'ın amacı sığ ağaçlarda bit israfını önlemek. Bedeli: bir CPtr'ın
anlamı ağacın şekline bağlı. Hata ayıklamak zor.

## Düzeltilen hata

Bu ADR'nin ilk hâli şunu öneriyordu: CPtr'ın üst 16 biti kök CNode'u
indeksler; o slot bir CNode ise alt 16 bit yaprağı indeksler, **değilse
alt 16 bitin sıfır olması beklenir ve slot'un kendisi döner.**

Bu tasarım tam olarak seL4'ün guard'ından kaçınmak için eleştirdiğim
belirsizliği üretiyor:

```
root[5] = Endpoint        ->  CPtr 5<<16 = 0x50000  Endpoint'i gösterir
root[5] silindi
root[5] = CNode (yeni)    ->  CPtr 0x50000 artık o CNode'un 0. slot'unu
                              gösterir. BAŞKA BİR NESNE. Hata yok.
```

Yani "aynı CPtr'ın hangi yarısının kullanıldığı ağacın şekline bağlı"
diye eleştirdiğim şeyi aynen yapmışım. İlk hâlin gerekçe bölümü kendi
tasarımını çürütüyordu ve bu fark edilmedi.

**Ders:** bir tasarımı başka bir tasarımın kusuruyla savunurken, aynı
kusurun kendi tasarımda farklı bir kılıkta olup olmadığına bakmak
gerekiyor. Burada kılık "L0 bazen kullanılır, bazen kullanılmaz" idi.

## Seçim: iki seviye *zorunlu*, artı nesil sayacı

İki değişiklik, ikisi de değişmezi bağımsız olarak koruyor.

### Değişiklik 1 — Yaprak cap'ler her zaman bir alt-CNode'da

**Kök CNode'un slot'ları yalnızca CNode cap'i taşıyabilir.** Başka bir
tip koymaya çalışmak `RootSlotMustBeCNode` hatası.

Böylece çözümleme her zaman iki adım: L1 ile kökten bir CNode seç, L0
ile o CNode'da bir slot seç. Dallanma yok, "slot CNode mi" sorusu yok.

```
CSpace:

  root CNode                 L1 = 2
  +--------+--------+--------+--------+
  | CNode  | CNode  | CNode  | (boş)  |
  +--------+--------+--------+--------+
                        |
                        v
                   +--------+--------+--------+
                   | EP     | Frame  | (boş)  |    L0 = 1
                   +--------+--------+--------+
                                ^
                   CPtr = (2 << 16) | 1
```

Bedeli: en az iki CNode gerekiyor, ve tek bir cap tutmak isteyen bir
bileşen bile bir alt-CNode ayırmak zorunda. Kabul edilebilir — bir CNode
en küçük hâlinde 32 × 2 = 64 bayt.

Kazancı: **CPtr'ın iki yarısının anlamı ağacın şeklinden bağımsız.**
L1 her zaman kökte, L0 her zaman yaprakta.

Alternatif düşünüldü: "tek seviye durumunda L0'ı anlamlı kıl", yani
kökteki bir yaprak cap'i `(index << 16) | 0` yerine `index` ile
adresle. O zaman L1=0 özel anlam kazanır ve kökün 0. slot'u
adreslenemez hale gelir — bir özel durum daha. Zorunlu iki seviye daha
az kural içeriyor.

### Değişiklik 2 — Nesil sayacı

Zorunlu iki seviye, *tip* değişiminden doğan belirsizliği kapatıyor.
Kapatmadığı: bir slot silinip **aynı tipte** başka bir nesneyle
doldurulursa eski CPtr yeni nesneyi gösterir. Klasik ABA.

Her slot bir **nesil sayacı** taşıyor ve CPtr onu içeriyor:

```
 63      56 55      48 47            32 31            16 15            0
+----------+----------+----------------+---------------+---------------+
| L1 nesil | L0 nesil |  rezerve (16)  |   L1 indeks   |   L0 indeks   |
|  8 bit   |  8 bit   |  sıfır olmalı  |    16 bit     |    16 bit     |
+----------+----------+----------------+---------------+---------------+
```

**Her slot için 8 bit nesil.** Slot boşaltıldığında (`delete`, ya da
`revoke` ile silindiğinde) sayaç artar, 255'ten 0'a sarar.

Çözümleme her iki seviyede de nesli karşılaştırıyor — bu yüzden "çift
nesil kontrolü": ara CNode'un kendisi değişmişse de yakalanır.

#### Neden 8 bit

Sarma kaçınılmaz; soru ne kadar sürede sardığı. 8 bit, bir slot'un 256
kez yeniden kullanılmasına izin veriyor. 256. kullanımda eski bir CPtr
tekrar geçerli görünebilir.

16 bit daha güvenli olurdu ama CPtr'da 32 bit nesil alanı demek, rezerve
alanın tamamı gider. 8 bit seçildi çünkü:

- Bir slot'un 256 kez döngüye girmesi, bir bileşenin o slot'u sürekli
  yeniden kullanması demek — kendi slot'u, kendi hatası.
- Asıl tehlikeli senaryo (A'nın B'ye bayat bir CPtr vermesi) birkaç
  yeniden kullanımda yakalanıyor; 256'yı bulması için kasıtlı bir çaba
  gerekir.
- 16 bit rezerve kalıyor, ki üçüncü bir seviye ya da CSpace etiketi için
  yer.

Bu bir **takas ve sınırlı bir garanti**; "ABA imkânsız" değil, "ABA için
256 yeniden kullanım gerekir" diyor. object-model.md'de böyle yazılı.

### Çözümleme

```
resolve(root, cptr) -> Result<&Slot, CPtrError>:
    if (cptr >> 32) & 0xFFFF != 0:        // rezerve alan
        return InvalidCPtr

    l1_index = (cptr >> 16) & 0xFFFF
    l0_index =  cptr        & 0xFFFF
    l1_gen   = (cptr >> 56) & 0xFF
    l0_gen   = (cptr >> 48) & 0xFF

    if l1_index >= root.slots:            return CPtrOutOfRange
    inner = &root[l1_index]

    if inner.generation != l1_gen:        return StaleCPtr
    if inner.is_empty():                  return EmptySlot
    if inner.type != CNode:               return RootSlotMustBeCNode

    cnode = inner.as_cnode()
    if l0_index >= cnode.slots:           return CPtrOutOfRange

    leaf = &cnode[l0_index]
    if leaf.generation != l0_gen:         return StaleCPtr

    return leaf
```

**En kötü durum: iki bellek erişimi, yedi karşılaştırma, döngü yok.**
Tahmini ≤ 25 komut. İlk hâlin 20 komutundan beş fazla; nesil
kontrollerinin bedeli bu.

### Değişmez

> **Bir CPtr, çözümlendiği her seferde ya aynı slot'u gösterir ya bir
> hata döndürür. Hiçbir koşulda başka bir slot'u göstermez.**

Üç mekanizma birlikte koruyor:

1. **Zorunlu iki seviye:** bitlerin anlamı ağacın şekline bağlı değil.
2. **Nesil sayaçları:** slot yeniden kullanılırsa eski CPtr `StaleCPtr`.
3. **Rezerve alan kontrolü:** üst bitlerde çöp olan bir CPtr reddedilir.

Kabul testi `cap-test` bunu doğruluyor (kontrol 10): bir slot silinip
farklı tipte bir nesneyle dolduruluyor, eski CPtr hata veriyor.

## Ne kaybediyoruz

**1. Derin CSpace'ler.** 2^32 slot adreslenebiliyor ama hiyerarşi tam
iki seviye. seL4'te bir bileşen alt bileşenine CSpace'inin bir dalını
verebilir ve o da dallanabilir; bizde alt bileşen düz kalır.

M13'e kadar sorun değil: init + birkaç servis + bir seri sürücü. Üç
kademeli bir bileşen hiyerarşisi bu kapıyı açmayı gerektirir — rezerve
16 bit orada duruyor.

**2. Minimum iki CNode.** Tek cap tutan bir bileşen bile kök + yaprak
istiyor. 64 bayt.

**3. 256 yeniden kullanımda nesil sarması.** Yukarıda.

**4. seL4 kodunun taşınabilirliği.** Zaten hedef değildi.

## Elenen alternatifler

**seL4'ün guard'lı radix ağacı.** En esnek, kanıtlanmış. Çözümleme bir
döngü içeriyor — derinliğe bağlı, yani en kötü durumu CSpace'in şekline
bağlı. "Kernel yolunda sınırsız döngü yok" kuralı CPtr çözümlemesini de
kapsıyor. *Not:* seL4'te derinlik CPtr'ın bit sayısıyla sınırlı (≤64
adım), yani teknik olarak sınırsız değil; ama 64 ile 2 arasındaki fark
IPC yolunda ölçülebilir.

**Tek seviye, düz dizi.** Bir karşılaştırma, bir indeksleme. CSpace
delegasyonunu tamamen imkânsız kılıyor.

**İlk hâl: opsiyonel ikinci seviye.** Yukarıda çürütüldü.

**Tek-seviyede L0'ı anlamlı kılmak.** L1=0'ı özel durum yapar, kökün 0.
slot'unu adreslenemez kılar. Zorunlu iki seviye daha az kural.

**Nesil sayacını atlamak ("protokol sorunu").** İlk hâlin duruşu ve
reddedildi: güvenlik hedefi olan bir sistemde kernel, yanlış nesneye
erişimi yapısal olarak imkânsız kılmalı — bileşenlerin dikkatine
bırakmamalı. Maliyeti slot başına 1 bayt ve çözümlemede iki
karşılaştırma.

**Nesil sayacını 16 bit yapmak.** Daha güvenli, rezerve alanın tamamını
yer. 8 bit + 16 bit rezerve seçildi.

**Ataların nesillerini de yürümek (soru 1'deki öneri).** O, derivation
ağacı boyunca 8 okuma demekti — ayrı bir mekanizma, revoke'u
ucuzlatmak için. Buradaki nesil kontrolü CSpace ağacında ve sabit iki
okuma; karıştırılmamalı.

## Sonuç

- CPtr 64 bit: 8 L1-nesil + 8 L0-nesil + 16 rezerve (sıfır) + 16 L1 +
  16 L0.
- **Tam iki seviye, zorunlu.** Kök CNode yalnızca CNode cap'i taşır.
- Çözümleme **döngüsüz**: iki bellek erişimi, yedi karşılaştırma,
  ≤ 25 komut.
- Guard yok. seL4'ten ayrıldığımız en büyük nokta.
- **Değişmez:** bir CPtr ya aynı slot'u gösterir ya hata verir; asla
  başka bir slot'u göstermez. Üç mekanizma koruyor, kabul testi
  doğruluyor.
- Nesil 8 bit: 256 yeniden kullanımdan sonra sarar. Sınırlı bir
  garanti, ve sınırı yazılı.
- **Kapattığımız kapı:** ikiden derin hiyerarşi. 16 rezerve bit orada.
- **Düzeltilen hata:** ilk hâl, kaçındığını iddia ettiği belirsizliği
  üretiyordu.