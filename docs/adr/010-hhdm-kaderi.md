# ADR 010 — HHDM'in kaderi: kendi fiziksel pencere eşlememizi kuruyoruz

Status: accepted (M3 architect pass)

## Bağlam

Kernel, paging açıkken çalışıyor ve bir fiziksel çerçeveye doğrudan
dokunamıyor. M2'de bu sorunu Limine'in HHDM'i (Higher Half Direct Map)
çözdü: bootloader tüm fiziksel belleği sabit bir offset'te eşliyor ve
offset'i bize söylüyor. Frame allocator bitmap'ini bu pencere üzerinden
yazıyor.

M3'te bir seçim var:

- **A.** Limine'in HHDM'ini sürdür. Bootloader'ın sayfa tablolarını devral,
  üzerine ekleme yap.
- **B.** Kendi sayfa tablolarımızı sıfırdan kur, kendi fiziksel pencere
  eşlememizi yap, CR3'ü değiştir.

İkisi de meşru. Seçimi belirleyen asıl soru [ADR 008](008-bootloader-reclaimable-dokunulmaz.md)'den
geliyor: **Limine'in sayfa tabloları `BOOTLOADER_RECLAIMABLE` içinde
duruyor.** O bölgeyi geri kazanmak istiyorsak, üzerinde çalıştığımız
eşlemeleri geri kazanmış oluruz. Yani A seçeneği, reclaimable'ı sonsuza
kadar dokunulmaz bırakmak demek.

## Seçim

**B: kendi sayfa tablolarımızı kuruyoruz.** Üç nedenle:

**1. Reclaimable'ı geri kazanmanın başka yolu yok.** Bootloader'ın
tablolarının üzerinde çalışırken o tabloları içeren belleği serbest
bırakmak, ayağın altındaki halıyı çekmektir. Kendi tablolarımıza geçip CR3'ü
değiştirdikten sonra Limine'in tabloları ölü veridir.

**2. Limine'in eşleme ayrıntıları protokolde tanımlı değil.** HHDM'in var
olduğu ve offset'i garanti; sayfa boyutu, NX biti, global bit, cache tipi
değil. Kendi penceremizi kurduğumuzda NX'in set olduğunu, MMIO'nun dışarıda
kaldığını, W^X'in geçerli olduğunu *biliyoruz* — tahmin etmiyoruz.

**3. M11'de zaten gerekecek.** User VSpace'ler kendi sayfa tablolarına sahip
olacak ve kernel yarısını paylaşacak. Paylaşılan kernel eşlemelerinin
bizim kurduğumuz, yapısını bildiğimiz tablolar olması şart.

### PMAP, HHDM ile aynı adreste

Kritik ayrıntı: kendi penceremizi **Limine'in HHDM'i ile aynı sanal adrese**
kuruyoruz (`0xFFFF_8000_0000_0000`, bkz. [ADR 009](009-adres-uzayi-yerlesimi.md)).

Sebep: geçiş anında daha önce hesaplanmış hiçbir adres geçersizleşmesin.
Frame allocator bitmap pointer'ını M2'de HHDM üzerinden hesapladı ve bir
`*mut u64` olarak saklıyor. CR3 değiştiğinde o pointer hâlâ aynı fiziksel
belleği göstermeli. Aynı adres seçilmezse geçişten hemen sonra her saklanmış
pointer'ı yeniden hesaplamak gerekirdi — ve biri unutulursa hata, geçişten
çok sonra ortaya çıkardı.

**Ama bu bir varsayım değil, doğrulanacak bir koşul.** Limine HHDM'i başka
bir adrese koymakta serbest. M3'te boot sırasında:

```
assert_eq!(hhdm_offset, PMAP_BASE,
           "Limine's HHDM is not where we build PMAP; the transition would \
            invalidate every pointer computed before it");
```

Bu assert tutmazsa kernel durur. Alternatif (farklı adrese kurup pointer'ları
yeniden hesaplamak) mümkün ama sessiz hata riski taşıyor; durmak daha
dürüst. Eğer bir gün gerçekten farklı bir HHDM ile karşılaşırsak, o zaman
bilinçli bir karar veririz.

### Geçiş sırası

M3'te şu sırayla:

1. Yeni bir PML4 için çerçeve ayır (frame allocator çalışıyor, HHDM hâlâ
   geçerli).
2. PMAP'i kur: tüm RAM'i `PMAP_BASE + phys`'e eşle, 2 MiB/1 GiB sayfalarla,
   NX + global, yazılabilir. **Yalnızca RAM** — framebuffer ve MMIO hariç.
3. Kernel imajını eşle, bölüm bölüm doğru izinlerle: `.text` RX,
   `.rodata` R, `.data`/`.bss`/`.limine_requests` RW+NX. W^X burada
   kazanılıyor; Limine imajı tek parça RWX eşliyor olabilir.
4. IST stack'lerini KSTACK'e taşı, altlarına **gerçek guard page** koy
   (eşlenmemiş). M1'den devreden ölü alan böylece gerçek korumaya dönüşür.
5. CR3'ü yeni PML4'e yaz. Bu noktada Limine'in tabloları ölü.
6. Tam TLB flush ([ADR 012](012-tlb-gecersizlestirme.md)).
7. Ancak şimdi reclaimable geri kazanılabilir
   ([ADR 013](013-reclaimable-geri-kazanim.md)).

Adım 5'in kendisi hassas: CR3 yazıldığı anda çalışmakta olan kodun eşlemesi
yeni tabloda da geçerli olmalı, yoksa bir sonraki komut getirme #PF üretir
ve IDT'nin kendisi de eşlenmemişse triple fault olur. Bu yüzden adım 3,
adım 5'ten önce ve kernel imajının tamamını kapsıyor.

## Elenen alternatifler

**A: Limine'in HHDM'ini sürdürmek.** En az iş. Bedeli: reclaimable sonsuza
kadar kayıp (~4 MiB), eşleme izinleri bootloader'ın insafına kalır (W^X
garantisi yok), ve M11'de user VSpace'ler için yine kendi tablolarımızı
kurmak gerekir — yani iş ertelenmiş olur, iptal edilmiş değil.

**Hibrit: Limine'in tablolarını devralıp üzerinde değiştirmek.** CR3 aynı
kalır, biz sadece eksik eşlemeleri ekler ve izinleri düzeltiriz.
Reclaimable yine kayıp (tablolar orada), ve Limine'in tablo yapısını
(hangi seviyede hangi sayfa boyutu kullanıldığını) keşfetmek gerekir —
protokolde tanımlı olmayan bir şeyi tersine mühendislikle anlamak.

**PMAP yerine geçici eşlemeler (temporary mapping window).** Fiziksel
belleğe erişim gerektiğinde bir sayfayı sabit bir pencereye eşle, kullan,
çöz. Adres alanından tasarruf eder — ama bizde adres alanı bol. Buna
karşılık her erişim bir eşleme + TLB invalidation; sıcak yollarda
kabul edilemez. Ayrıca iç içe erişim (bir sayfa tablosunu okurken başka
birine bakmak) pencere yönetimini karmaşıklaştırır.

**PMAP'i 4 KiB sayfalarla kurmak.** 256 MiB için 64K PTE = 512 KiB tablo;
64 TiB için imkânsız. 2 MiB sayfalar bunu 128 KiB'a indirir, 1 GiB sayfalar
daha da. Büyük sayfalar ayrıca TLB basıncını azaltır. 4 KiB'ın tek avantajı
sayfa başına izin ayarı — PMAP'te buna ihtiyaç yok, hepsi aynı (RW+NX).

## Sonuç

- M3'ten sonra kernel kendi sayfa tablolarında çalışıyor; Limine'in
  tabloları ölü veri.
- PMAP `0xFFFF_8000_0000_0000`'da, HHDM ile aynı adreste. Geçiş hiçbir
  pointer'ı geçersizleştirmiyor. Bu bir assert ile doğrulanıyor, varsayım
  değil.
- PMAP NX ve yalnızca RAM içeriyor; MMIO ayrı aralıkta
  ([ADR 009](009-adres-uzayi-yerlesimi.md)).
- Kernel imajı bölüm bölüm doğru izinlerle eşleniyor: W^X kazanıldı.
- IST stack'lerinin altında gerçek guard page var; M1'den devreden ölü
  alan artık #PF üretiyor.
- `BOOTLOADER_RECLAIMABLE` geri kazanılabilir hale geliyor
  ([ADR 013](013-reclaimable-geri-kazanim.md)).
- `memory::hhdm_offset()` M3'ten sonra "Limine'in söylediği" değil "bizim
  kurduğumuz PMAP tabanı" anlamına gelecek; isim `pmap_base()` olarak
  değişecek ve `hhdm_offset()` yalnızca boot doğrulamasında kalacak.
- Bedeli: ~200 satır sayfa tablosu kurulum kodu ve geçişin kendisi, ki
  hatası triple fault. Karşılığında eşlemelerin tamamı bizim ve
  denetlenebilir.