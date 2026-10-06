# ADR 019 — Derivation tree ve revoke: sınırı adlandırmak

Status: accepted (M5 architect pass)

Kaynak: seL4 Reference Manual bölüm 3.3 ("Deletion and Revocation") ve
seL4 kaynak kodundaki MDB (mapping database). Bu ADR'nin asıl konusu,
seL4'ün *çözmediği* bir problemi nasıl ele aldığımız.

## Bağlam

`revoke(cap)` bir capability'den **türeyen her şeyi** silmeli. Türeme iki
yolla olur:

- **retype:** bir Untyped'dan nesne üretmek. Üretilen nesnenin cap'i,
  Untyped cap'inin çocuğudur.
- **copy:** bir cap'i başka bir slot'a kopyalamak (muhtemelen hakları
  daraltarak). Kopya, kaynağın çocuğudur.

Revoke'un var olma sebebi: **yetkiyi geri alabilmek.** Bir bileşene bir
Endpoint verdiysem ve o da başkalarına kopyaladıysa, revoke hepsini
siler. Bu olmadan capability modeli tek yönlü bir kapı olurdu.

İki şey gerekiyor:

1. "Türedi" ilişkisinin bir yerde saklanması.
2. Revoke'un maliyetinin bilinmesi — ve bu projede, `master prompt`
   gereği, **sınırlı olması**.

İkincisi sorunlu. seL4'te revoke sınırsızdır ve bu bilinen bir
problemdir: bir milyon türemiş cap varsa, revoke bir milyon adım sürer.
seL4 bunu "preemption point" ile hafifletir (işlem bölünebilir) ama
toplam maliyeti sınırlamaz.

## Nerede saklanıyor: slot'un içinde, çift bağlı liste

seL4'ün MDB'si ile aynı: her cap slot'u iki işaretçi taşır ve tüm
cap'ler tek bir **derivation sırasına** göre bağlı listede durur. Sıra,
derivation ağacının derinlik-öncelikli ön-sıralamasıdır (preorder).

```
slot = { object: 8B, type+rights: 8B, mdb_prev: 8B, mdb_next: 8B } = 32 B
```

Bu, [ADR 017](017-nesne-kumesi.md)'deki 32 bayt/slot tahmininin
gerekçesi.

Preorder'ın faydası: **bir cap'in tüm torunları, listede hemen onun
ardından ve kesintisiz gelir.** Yani revoke = "şu slot'tan başla, torunu
olmayan ilk slot'a kadar ilerle, hepsini sil". Ağaç yürüyüşü yok, işaretçi
takibi yok; tek yönlü bir liste taraması.

Alternatif (ayrı bir ağaç yapısı, çocuk/kardeş işaretçileri) daha az yer
tutmaz ve yürüyüşü karmaşıklaştırır. Slot zaten var; iki işaretçi eklemek
en ucuz yol.

### "Torunu mu" testi

Liste komşuluğu yetmez: listedeki bir sonraki slot, torun da olabilir
kardeş de. seL4 bunu her cap'in bir **derivation depth** alanı ile
çözer. Biz de öyle yapıyoruz — ve derinliği zaten sınırlıyoruz, aşağıya
bakın.

```
revoke(root):
    depth = root.depth
    cursor = root.mdb_next
    while cursor != null and cursor.depth > depth:
        delete(cursor)              # nesneyi değil, cap'i siler
        cursor = cursor.mdb_next
```

`cursor.depth > depth` yanlış olduğu an, torunlar bitmiştir.

## Derinlik sınırı: 8

**Bir cap en fazla 8 seviye derinde türeyebilir.** `copy` ve `retype`,
sonuç 8'i aşacaksa `DerivationTooDeep` döndürür.

Gerçekçi zincir: Untyped → alt Untyped → alt Untyped → nesne → kopya →
daraltılmış kopya. Beş. Sekiz, rahat bir tavan.

Bu sınır üç şeyi birden mümkün kılıyor:

1. **`depth` alanı 3 bite sığıyor** — tip ve hakların yanına, ayrı bir
   kelime gerekmeden.
2. **Bir cap'in atalarını yürümek sınırlı:** en fazla 8 adım. Aşağıdaki
   revocation kilidi bunu kullanıyor.
3. **Patolojik derinlik imkânsız:** kötü niyetli bir bileşen, revoke'u
   yavaşlatmak için derin bir zincir kuramaz. (Geniş bir ağaç kurabilir;
   o ayrı bir konu ve aşağıda.)

## Revoke'un maliyeti: sayı

Derinlik sınırlı ama **genişlik sınırlı değil.** Bir Untyped'dan 65536
Frame retype edilebilir ve hepsi bir çocuktur. Revoke hepsini gezer.

En kötü durum, sistemdeki toplam slot sayısıyla sınırlı:

| Büyüklük | Değer |
|---|---|
| Slot boyutu | 32 B |
| 256 MiB makinede slot tavanı | 8.4 milyon |
| Slot başına revoke maliyeti | ~20 komut (oku, sil, ilerle) |
| **En kötü durum toplam** | **~168 milyon komut** |
| 3 GHz'de | **~56 ms** |

**56 milisaniye.** Bu sayı, bu tasarımın en rahatsız edici gerçeği ve
gizlemek yerine yazıyorum.

Pratikte çok daha küçük: M13'ün init sistemi birkaç yüz cap tutacak,
yani revoke mikrosaniyeler sürecek. Ama *en kötü durum* budur ve bir
bileşen onu kasten üretebilir.

### Ölçüm (M5 uygulaması): tahmin 120 kat iyimserdi

Yukarıdaki tablo mimari geçişte hesaplandı ve hiçbir şeye karşı
doğrulanmadı. `cap-revoke-depth` senaryosu artık onu ölçüyor: init
CSpace'in tutabildiği en geniş ve en derin ağacı kurar (256 cap, 8
seviye), revoke eder, TSC ile sayar.

| | Tahmin | Ölçüm |
|---|---|---|
| Cap başına maliyet | ~20 komut | **2424 TSC çevrimi** |
| 8.4M slot için | ~168M komut | ~20.4 milyar çevrim |
| 3 GHz'de | **~56 ms** | **~6787 ms** |

**Yüz yirmi kat.** Bu farkın tamamı tasarım hatası değil; ayırmak
gerekiyor:

- **Emülasyon.** Ölçüm QEMU'nun TCG yorumlayıcısında yapıldı ve TCG
  tipik olarak 10–50 kat yavaştır. Üstelik QEMU'da `rdtsc` gerçek bir
  saat değil, sanal bir sayaçtır.
- **Önbellek.** 256 slotluk bir ağaç 8 KiB'dir ve L1'e sığar. 8.4
  milyon slot 268 MiB'dir; her adım bir önbellek ıskası olur. Yani
  gerçek en kötü durumun cap başına maliyeti ölçülenden **daha
  yüksek** olabilir, düşük değil.
- **Tahminin kendisi.** "20 komut" sayısı okuma, silme ve ilerlemeyi
  sayıyordu; `unlink`'in iki komşuyu güncellemesini, `clear`'ın nesil
  sayacını ve sınır kontrollerini saymıyordu. Gerçek sayı muhtemelen
  40–60 civarı.

Dürüst sonuç: **en kötü durum 56 ms değil.** Gerçek donanımda
ölçülmeden tek sayı söylemek doğru olmaz, ama 56 ms'in bir alt sınır
olduğu ve gerçeğin en az birkaç yüz milisaniye olacağı açık.

Bu, yukarıdaki (c) seçimini — "kesilebilir ama bölünemez" — zayıflatıyor
değil, **daha pahalı hale getiriyor.** 56 ms boyunca thread çalışmaması
kabul edilebilir görünüyordu; yarım saniye görünmüyor. M10'da bu karar
yeniden ele alınacaktı; artık yeniden ele alınması **gerekiyor.**

Dersi ayrıca kaydetmeye değer: *mimari geçişte hesaplanan bir sayı, bir
ölçüm değildir.* ADR'nin "Sayı hesaplandı, kabul edildi, yazıldı"
cümlesi doğruydu ama yeterli değildi; eksik olan "ve doğrulanmadı"
idi. M6'dan itibaren bu tür her tahminin yanına onu ölçen bir senaryo
konacak.

## "Sınırsız döngü yok" kuralıyla ne yapıyoruz

Kural şu: *kernel yolunda sınırsız döngü yok.* Revoke'un döngüsü
sınırlıdır — toplam slot sayısı kadar — ama o sınır çok büyük.

Üç seçenek vardı:

**(a) seL4 gibi: uzun sürsün, bölünebilsin.** İşlem preemption
noktalarında bölünür, araya başka thread girer. Problem: araya giren
thread derivation ağacını değiştirebilir ve revoke'un durumu bozulur.
seL4 bunu MDB'nin tutarlılığını koruyarak çözer, ama karmaşıklık yüksek.

**(b) Bölünebilir revoke, kullanıcı tekrar çağırır.** `revoke` en fazla
N slot siler, "tamamlanmadı" döner, çağıran döngüye girer. Basit ama
**güvenlik açığı**: yarım revoke, yetkinin bir kısmının hâlâ dışarıda
olması demek. Ve çağıran döngüyü kurmazsa yetki orada kalır. Revoke'un
tek işi yetkiyi geri almak; "belki geri alır" bir revoke değildir.

**(c) Kesilebilir ama bölünemez.** Seçtiğimiz.

### Seçim: kesmelere açık, thread değişimine kapalı

Revoke, syscall içinde **tamamlanana kadar** çalışır. Bu sürede:

- **Kesmeler açık ve işleniyor.** Bir cihaz kesmesi gelirse handler
  çalışır, Notification sinyallenir, `iretq` ile revoke'a dönülür. Cihaz
  verisi kaybolmaz.
- **Thread değişimi yok.** Zamanlayıcı bu syscall'ın ortasında başka bir
  thread'e geçmez. Dolayısıyla derivation ağacı revoke sürerken
  değişemez ve tutarlılık sorunu doğmaz.

Bedeli açık: **revoke süren 56 ms boyunca hiçbir thread çalışmaz.**
Gerçek zamanlı bir sistem için kabul edilemez; bu kernel M13'te gerçek
zamanlı değil (bkz. [ADR 017](017-nesne-kumesi.md), SchedContext
alınmadı). Tutarlılık, latency'ye tercih edildi.

**M10'da yeniden ele alınacak.** Zamanlayıcı geldiğinde bu karar
ölçülebilir hale gelir: gerçek bir iş yükünde revoke ne kadar sürüyor?
Eğer milisaniyeler buluyorsa (a) seçeneğine geçmek gerekir ve o zaman
MDB tutarlılığı için aşağıdaki kilit mekanizması hazır duruyor.

### Hazır bırakılan kilit mekanizması

(a)'ya geçiş gerekirse: her slot'ta bir `revoking` biti, ve `copy` /
`retype` çağrıldığında **atalardan herhangi birinde** bu bit set mi diye
bakılır. Derinlik 8 ile sınırlı olduğu için bu kontrol **en fazla 8
bellek okuması** — türetme yolunda kabul edilebilir, çözümleme yolunda
(sıcak yol) hiç yok.

Bu biti şimdi tanımlıyorum ama kullanmıyorum. Yer ayrılmış olması,
geçişi lokal bir değişiklik yapıyor.

## delete ve revoke farkı

- `delete(slot)`: **tek** bir cap'i siler. Eğer o cap bir nesnenin son
  cap'iyse, nesne de yok edilir ve belleği Untyped'a döner
  ([ADR 020](020-retype-semantigi.md)).
- `revoke(slot)`: slot'un **torunlarını** siler, slot'un kendisini
  bırakır. Yani "verdiklerimi geri al, benimki kalsın".

seL4 ile aynı ayrım. `revoke` + `delete` birlikte "bu yetkiyi tamamen
yok et" demek.

## Elenen alternatifler

**Ağacı slot dışında tutmak (ayrı bir tablo/ağaç).** Slot'u 16 bayta
indirirdi ama tabloyu bir yerde tutmak gerekir — ve o yer de bellek, ve
cap sayısıyla büyüyor. Üstelik slot ile derivation kaydı arasında bir
eşleme gerekir. Çift bağlı liste slot'un içinde bedavaya geliyor.

**Çocuk sayacı ile revoke'u sınırlamak.** Her cap "en fazla N çocuk"
diye sınırlansaydı revoke O(N^depth) olurdu — 8 derinlikte N=16 bile
4 milyar. Sınır değil, yanılsama.

**Revoke'u yalnızca Untyped'a izin vermek.** Revoke'u "bu Untyped'ın
bellek aralığına işaret eden tüm cap'leri bul ve sil" haline getirir.
Maliyet O(sistemdeki tüm slot'lar) — yani hep en kötü durum. Daha kötü.

**Nesil (generation) sayacı ile O(1) revoke.** Her cap, türediği
atanın nesil numarasını saklar; revoke atanın neslini artırır ve tüm
torunlar bir anda geçersizleşir. Çekici. Problemi: her *lookup*'ta
atalar zincirinin nesillerini doğrulamak gerekir — 8 okuma, sıcak yolda.
CPtr çözümlemesini 20 komuttan ~60'a çıkarır
([ADR 018](018-cptr-ve-cspace.md) bunu 20'de tutmayı amaçlıyordu).
Revoke'u ucuzlatıp her syscall'ı pahalılaştırmak yanlış takas.
**Ama bu, açık sorulardan birinin konusu (soru 1).**

**Revoke'u asenkron yapmak (arka planda temizlik).** Yetki, temizlik
bitene kadar dışarıda kalır. Revoke'un anlamını kaybeder.

## Sonuç

- Derivation, slot içindeki çift bağlı listede, preorder sırada. Slot
  32 bayt.
- Torun testi `depth` alanıyla; derinlik **en fazla 8**, 3 bite sığıyor.
- Revoke = listede ileri tara, `depth > root.depth` olduğu sürece sil.
  Ağaç yürüyüşü yok.
- **En kötü durum tahmini 56 ms idi; ölçüm bunu 120 kat aştı**
  (`cap-revoke-depth`: cap başına 2424 TSC çevrimi, emülasyon altında).
  Farkın bir kısmı TCG, bir kısmı tahminin eksik komut sayması. Gerçek
  sayı bilinmiyor ama 56 ms'ten büyük.
- Revoke **kesmelere açık, thread değişimine kapalı**: cihaz verisi
  kaybolmaz, derivation ağacı değişemez. Bedeli: o süre boyunca başka
  thread çalışmaz.
- **M10'da yeniden ele alınması artık isteğe bağlı değil**: ölçüm,
  "kabul edilebilir latency" varsayımını geçersiz kıldı.
- Bölünebilir revoke'a geçiş için `revoking` biti ve 8 adımlık ata
  kontrolü tasarlandı, uygulanmadı.
- **Açık:** nesil sayacı ile O(1) revoke, CPtr çözümlemesini
  pahalılaştırma pahasına. Soru 1.