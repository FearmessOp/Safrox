# ADR 015 — `unmap` ara sayfa tablolarını serbest bırakmıyor

Status: **closed** — [ADR 027](027-vspace-modeli.md) (M7) user
tarafını, [ADR 043](043-user-space-vspace-yonetimi.md) (M11) kernel
tarafını kapattı.

İlgili: [ADR 011](011-sayfa-tablosu-api.md),
[ADR 027](027-vspace-modeli.md),
[ADR 043](043-user-space-vspace-yonetimi.md)

## Kapanış (M11)

Soru şuydu: `unmap` ara sayfa tablolarını serbest bırakmalı mı?

M7 user tarafını kapattı — tablolar artık PageTable capability'si ve
`unmap_table` onları sahibine geri veriyor. Kernel tarafı açık
kalmıştı ve M11'de kapandı. Cevap "serbest bırakmıyoruz" değil,
**"sorunun bir örneği yok"**:

Kernelin sayfa tabloları boot'ta kuruluyor ve hiç kaldırılmıyor.
Heap yalnızca büyüyor (ADR 016), KSTACK slotları sabit, PMAP ve
image eşlemeleri kalıcı. `unmap` kernel yarısında **çağrılmıyor**.

Bir gün çağrılırsa cevap user tarafının cevabı olacak: tabloyu
sahibine geri ver. O yığın bir TCB'ye ait, TCB bir Untyped'dan
geliyor, ve Untyped revoke edildiğinde tablolar da dönüyor —
ADR 020'nin watermark kararı bunu zaten çözüyor.

Yani sorunun doğru hâli "kernel bir tabloyu ne zaman serbest
bırakmalı" değil, **"kernel bir tabloya hiç sahip olmamalı"** idi.
Kapsam yoluyla kapanıyor, uygulama yoluyla değil — ve bu bir
erteleme değil, bir cevap.

## Bağlam

`paging::map` bir sayfayı eşlerken yol üzerindeki eksik tabloları kuruyor:
PML4 girdisi boşsa bir PDPT, o boşsa bir PD, o boşsa bir PT ayırıyor. Bu
zorunlu — dört seviyeli sayfalamada yaprak girdiye ulaşmanın başka yolu
yok.

`paging::unmap` ise yalnızca yaprak girdiyi temizliyor. Ara tablolar
ayrılmış kalıyor, içleri boş olsa bile.

Bu, M3'ün kabul testinde somut bir sayı olarak görünüyor: `vm-test`
senaryosu `0xFFFF_D800_0000_0000` adresinde bir sayfa eşleyip çözüyor ve
sonrasında **üç çerçeve** ayrılmış durumda kalıyor — o adrese özel PDPT,
PD ve PT.

İlk yazdığım test bunu sızıntı sayıp panic etti. Test yanlıştı, kod değil;
ama "yanlış" demek yetmez, kararın gerekçesi yazılmalı.

## Seçim

**Ara tablolar serbest bırakılmıyor. M5'e kadar.**

Bir tabloyu serbest bırakmak için onun *boş* olduğunu bilmek gerekir. Üç
yol var:

**a) Serbest bırakmadan önce tabloyu tara.** 512 girdiye bak, hepsi boşsa
bırak. Doğru ama her `unmap` çağrısını O(512)'ye çıkarır, ve üst
seviyelerde bu tekrarlanır (PT boşsa PD'yi tara, o da boşsa PDPT'yi...).
Sıcak bir yolda kabul edilemez.

**b) Tablo başına referans sayacı.** Her tabloda kaç canlı girdi olduğunu
tut; sıfıra inince bırak. Standart çözüm, ve M5'te gelecek olan bu. Ama
sayacın doğru olması gerekir, ve **çözme yönünde yanlış bir sayaç,
başka bir eşlemenin hâlâ içinden yürüdüğü bir tabloyu serbest bırakır.**
Sonuç: sayfa yürüyücüsünde use-after-free, yani keyfi bellek bozulması,
teşhisi neredeyse imkânsız.

**c) Bırakma.** M3'ün seçimi.

### Bedel

Dokunulan her 512 GiB'lık adres aralığı başına üç çerçeve — 12 KiB. M3'te
kernel toplam birkaç ayrı aralığa dokunuyor (imaj, KSTACK, test
scratch'leri), yani ölçülebilir maliyet onlarca kilobayt.

Karşılığında: sayfa yürüyücüsünde use-after-free olmaması.

Bu takas M3 için apaçık. M5'te öyle olmayacak — thread'ler gelip gidecek,
her biri kendi yığın aralığıyla, ve o zaman sayaçlar gerekecek. O noktada
sayaçlar kapability nesne modelinin bir parçası olarak gelecek, yani
tablolar zaten sahiplenilmiş nesneler olacak ve "kaç referans var"
sorusunun cevabı modelin kendisinde duracak.

### Sessizce kabul etmek yerine iddia etmek

Testin "üç çerçeve fazla, boşver" demesi kolay olurdu. Bunun yerine tam
sayı iddia ediliyor:

```rust
const INTERMEDIATE_TABLES: u64 = 3;
let still_out = before.saturating_sub(after);
assert_eq!(
    still_out, INTERMEDIATE_TABLES,
    "expected exactly {} frames to remain allocated (the PDPT, PD and \
     PT backing {}), but {} are out; a leaf frame leaked or an \
     unnecessary table was built",
    INTERMEDIATE_TABLES, page, still_out
);
```

Fark: "biraz fazla olabilir" diyen bir test, `unmap`'in yaprak çerçeveyi
de bırakmayı unuttuğu günü fark etmez. Tam sayı iddia eden test eder.

Aynı mantık `memtest`'in boot yapılarını sayan kontrolünde de var, orada
bir üst sınırla: makinenin memory map'ine göre PMAP'in kaç PD'ye ihtiyaç
duyduğu değişiyor, yani tam sayı taşınabilir değil. Sınır yine de
ilginç hatayı yakalıyor — boot kodunun tablo başına değil çerçeve başına
ayırması.

## Elenen alternatifler

**Tarama (a).** Yukarıda. O(512) per unmap, ve seviyeler boyunca
tekrarlanır.

**Şimdi referans sayacı yazmak (b).** Cazip, çünkü er geç gerekecek. Ama
sayaçların doğruluğu, tabloların *kim tarafından sahiplenildiğine* bağlı
ve o model M5'te geliyor. Şimdi yazılacak sayaçlar, sahiplik modeli
gelince yeniden yazılacak — ve arada geçen sürede hatalı bir sayaç,
bulması en zor hata sınıfını üretiyor olacak.

**Tembel toplama: ara sıra tüm tabloları tarayıp boşları topla.** Bir
GC. Ne zaman çalışacağı belirsiz, ve çalıştığı an sayfa tablolarını
kilitlemesi gerekir. M3'te çözdüğü sorun (onlarca KiB) problemin kendisi
olmaktan uzak.

## Sonuç

- `unmap` yaprak girdiyi temizliyor, ara tabloları bırakmıyor.
- Maliyet: dokunulan her 512 GiB başına 12 KiB.
- Gerekçe: referans sayacı olmadan serbest bırakmak, sayfa yürüyücüsünde
  use-after-free riski taşır — bellek israfından çok daha pahalı bir hata.
- `vm-test` tam sayıyı (3) iddia ediyor, "yaklaşık" değil; böylece yaprak
  çerçevenin sızdığı gün fark edilir.
- M5'te kapability nesne modeliyle birlikte sayaçlar gelecek ve bu karar
  yeniden ele alınacak.

## M7 eki: borç kapatıldı, ama ödenerek değil

Yukarıdaki son madde iki milestone geç kaldı. M5 geldi ve sayaçlar
gelmedi; borç M7'ye devroldu. Burada kapanıyor, ve **kapanma biçimi
beklenenden farklı.**

Bu ADR bir soru soruyordu: *kernel bir ara tabloyu ne zaman güvenle
serbest bırakabilir?* Üç cevabı değerlendirdi (tara, say, bırakma) ve
üçüncüsünü seçti.

M7'de soru **ortadan kalktı.** Ara tablolar artık PageTable
capability'leri (ADR 027). `Map` çağıranın sağladığı bir tabloyu
ağaca yerleştiriyor; `Unmap` onu çıkarıyor ve capability sahibinde
kalıyor. Kernel hiçbir zaman serbest bırakmıyor, çünkü hiçbir zaman
sahip değil.

"Kaç referans var" sorusunun yerini "bu tablo ağaçta mı" aldı, ve o
tek bir girdi kontrolü. Sayaç gerekmiyor, tarama gerekmiyor, ve
korkulan use-after-free yapısal olarak imkânsız: bir tablonun
belleğini geri almanın tek yolu `revoke`, ve revoke her şeyi birden
yıkıyor.

Bu, kernel'den politika çıkarmanın ADR 011'den beri süren çizgisinin
bir örneği. **Bir kaynak yönetimi sorusu çözülemiyorsa, kaynağın
sahibini değiştirmek sorunun kendisini kaldırabilir.**

### Hâlâ açık olan kısım

Kernel'in **kendi yarısındaki** ara tablolar hâlâ bırakılmıyor, ve
orada sahip kernel olduğu için soru aynen duruyor.

Pratik bir örneği yok: kernel eşlemeleri boot'ta kuruluyor ve
kaldırılmıyor, heap yalnızca büyüyor (ADR 016), ve KSTACK slotları
sabit. Yani bu yolda hiç `unmap` çağrılmıyor.

Bir gün çağrılırsa — bir thread öldüğünde stack'inin çözülmesi en
olası aday, M9 — soru olduğu gibi geri gelecek. Bilinen eksiklere
yazıldı, ve o zaman bu ADR'nin üç seçeneği hâlâ geçerli olacak.