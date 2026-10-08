# PythOS adres uzayı haritası

Status: fixed at M3 (architect pass). Bu haritadaki **user/kernel sınırı ve
kernel aralıklarının başlangıç adresleri kalıcıdır**; M8'de user space
geldiğinde değişmeyecek. Gerekçeler
[ADR 009](adr/009-adres-uzayi-yerlesimi.md) ve
[ADR 010](adr/010-hhdm-kaderi.md)'da.

x86-64, 4 seviyeli sayfalama, 48-bit sanal adres. Kanonik adres uzayı iki
yarıya bölünür ve ortada 16 EiB'lık bir boşluk vardır: bit 47'den bit 63'e
kadar olan bitlerin hepsi bit 47 ile aynı olmalıdır.

Referans: Intel SDM Vol. 1, 3.3.7.1 "Canonical Addressing"; Vol. 3A, 4.5
"4-Level Paging".

## Harita

```
================= USER HALF (kanonik alt yarı, 128 TiB) ==================

0x0000_0000_0000_0000 .. 0x0000_0000_0000_FFFF   64 KiB   null guard
    Hiçbir zaman eşlenmez. Bir null pointer dereference'ı #PF üretsin diye.
    M8'de user VSpace kurulurken bu aralık açıkça boş bırakılır.
    Sahip: kimse (rezerve)                              Dolacak: asla

0x0000_0000_0001_0000 .. 0x0000_7FFF_FFFF_FFFF   ~128 TiB user
    Kullanıcı adres uzayı. Yerleşimi kernel'in işi DEĞİL: hangi sayfanın
    nereye eşleneceğine user-space bileşeni karar verir ve kernel'e
    capability invocation ile söyler. Kernel yalnızca "bu aralık user'a
    ait" sınırını bilir.
    Sahip: user VSpace                                  Dolacak: M8 / M11

--------- KANONİK BOŞLUK: 0x0000_8000_.. .. 0xFFFF_7FFF_.. ---------------
    Mimari olarak erişilemez. Bu boşluğa düşen bir adres #GP üretir, #PF
    değil -- kanoniklik kontrolü sayfa yürüyüşünden önce yapılır.

================ KERNEL HALF (kanonik üst yarı, 128 TiB) =================

0xFFFF_8000_0000_0000 .. 0xFFFF_BFFF_FFFF_FFFF   64 TiB   PMAP
    Fiziksel bellek penceresi. Tüm fiziksel RAM buraya doğrusal eşlenir:
        virt = PMAP_BASE + phys
    Kernel'in bir fiziksel çerçeveye dokunmasının tek yolu. 2 MiB (mümkün
    olduğunda 1 GiB) sayfalarla eşlenir, NX + global.
    Limine'in HHDM'i ile AYNI adreste başlar, bilerek: M3'te geçiş
    yapıldığında hiçbir önceden hesaplanmış adres geçersizleşmez.
    Sahip: kernel                                       Dolacak: M3

0xFFFF_C000_0000_0000 .. 0xFFFF_C7FF_FFFF_FFFF    8 TiB   KHEAP
    Kernel heap. M4'te dolduruldu, ama 8 TiB'ın tamamı değil:

        0xFFFF_C000_0000_0000   alt guard sayfası (eşlenmez)
        0xFFFF_C000_0000_1000   heap başlangıcı
        ...                     talep üzerine eşlenir, büyür
        0xFFFF_C000_0400_1000   64 MiB sınırı
        0xFFFF_C000_0400_1000   üst guard sayfası (eşlenmez)

    64 MiB bir rezervasyon değil, bir TAVAN: sayfalar talep üzerine
    eşleniyor ve heap hiç küçülmüyor. Sınır, kaçak bir ayırma döngüsünün
    makinenin bütün çerçevelerini yutup alakasız bir yerde çökmesi yerine
    okunabilir bir panic ile bitmesi için var.
    Gerisi (8 TiB - 64 MiB) boş; sınır ileride yükseltilebilir.
    Sahip: kernel                                       Doldu: M4

0xFFFF_C800_0000_0000 .. 0xFFFF_CFFF_FFFF_FFFF    8 TiB   KSTACK
    Kernel stack'leri. Her stack'in altında eşlenmemiş bir guard page.
    Thread başına bir stack; TCB sayısı kadar.
    M3'te yalnızca IST stack'leri buraya taşınır (guard page ile birlikte);
    gerisi TCB'lerle gelir.
    Sahip: kernel                                       Dolacak: M3 / M5

0xFFFF_D000_0000_0000 .. 0xFFFF_D7FF_FFFF_FFFF    8 TiB   KMMIO
    Cihaz belleği. PMAP'ten ayrı, çünkü MMIO'nun cache tipi farklı
    (UC/WC) ve bir cihaz penceresini RAM'le aynı tabloda tutmak yanlış
    cache tipiyle eşleme riskini doğurur.
    Sahip: kernel                                       Dolacak: M6 / M7

0xFFFF_D800_0000_0000 .. 0xFFFF_FFFF_7FFF_FFFF   ~40 TiB  (rezerve)
    Boş. İleride ihtiyaç duyulacak her şey (per-CPU alanlar, vmalloc
    benzeri bir aralık, kernel modülleri) buradan tahsis edilir.
    Sahip: -                                            Dolacak: -

0xFFFF_FFFF_8000_0000 .. 0xFFFF_FFFF_FFFF_FFFF    2 GiB   KERNEL IMAGE
    Kernel imajı: .text, .rodata, .limine_requests, .data, .bss, IST
    stack'leri. Adres `-C code-model=kernel`in zorunlu kıldığı yer:
    32-bit işaret genişletilmiş yer değiştirmelerle erişilebilen tek
    aralık bu.
    Sahip: kernel                                       Dolacak: M0 (dolu)
```

## Sabitler

Koddaki karşılıkları (M3'te `kernel/src/memory/layout.rs`):

| Sabit | Değer | Anlam |
|---|---|---|
| `USER_MIN` | `0x0000_0000_0001_0000` | User alanının ilk geçerli adresi |
| `USER_MAX` | `0x0000_7FFF_FFFF_FFFF` | User alanının son geçerli adresi |
| `KERNEL_MIN` | `0xFFFF_8000_0000_0000` | Kernel yarısının ilk adresi |
| `PMAP_BASE` | `0xFFFF_8000_0000_0000` | Fiziksel pencere tabanı |
| `PMAP_SIZE` | `64 TiB` | Pencere boyutu |
| `KHEAP_BASE` | `0xFFFF_C000_0000_0000` | Heap alanının tabanı (alt guard) |
| `KHEAP_START` | `0xFFFF_C000_0000_1000` | Heap'in ilk kullanılabilir baytı |
| `KHEAP_MAX_SIZE` | `64 MiB` | Heap'in büyüyebileceği tavan |
| `KHEAP_END` | `0xFFFF_C000_0400_1000` | Tavanın bir üstü |
| `KHEAP_GUARD_TOP` | `0xFFFF_C000_0400_1000` | Üst guard sayfası |
| `KSTACK_BASE` | `0xFFFF_C800_0000_0000` | Kernel stack alanı tabanı |
| `KMMIO_BASE` | `0xFFFF_D000_0000_0000` | MMIO alanı tabanı |
| `KIMAGE_BASE` | `0xFFFF_FFFF_8000_0000` | Kernel imajı tabanı (linker.ld) |

## Değişmezler

Bunlar M3'te derleme zamanı `assert!`leri, çalışma zamanı kontrolleri ve
link sonrası denetimle zorlanır. Her maddenin yanında onu *hangi şeyin*
zorladığı yazıyor — zorlanmayan bir değişmez, değişmez değil dilektir.

1. **Hiçbir user adresi `KERNEL_MIN`'in üstünde olamaz.** Tersi de geçerli:
   hiçbir kernel aralığı `USER_MAX`'ın altına inmez. Sınır tek bir
   karşılaştırmayla kontrol edilebilir.
   *Zorlayan:* `layout.rs` içindeki `const _: ()` blokları (derleme zamanı).

2. **PMAP yalnızca RAM içerir.** MMIO ve framebuffer PMAP'e girmez; KMMIO'ya
   eşlenir. Bir cihaz penceresini PMAP üzerinden okumak mümkün olmamalı.
   *Zorlayan:* `vm::is_real_ram`, memory map tipine göre filtreliyor.

3. **PMAP sayfaları NX'tir.** Veri olarak eşlenen hiçbir şey yürütülemez.
   *Zorlayan:* `vm::init` pencereyi `READ|WRITE|GLOBAL` ile kuruyor (EXEC
   yok); `vm-nx` senaryosu PMAP'teki bir sayfadan yürütmeyi deniyor ve #PF
   alıyor (hata kodu 0x11, komut getirme biti set).

4. **Hiçbir kernel *sayfası* hem yazılabilir hem yürütülebilir değildir**
   (W^X), ve `paging::map` böyle bir bayrak çiftini `InvalidFlags` ile
   reddeder.

   **İstisna — PMAP takma adı.** Kernel imajının fiziksel çerçeveleri,
   PMAP üzerinden *ayrıca* yazılabilir olarak görünür. Hiçbir tek sayfa
   W∧X değil, ama `.text`'in baytları iki yoldan erişilebiliyor: imaj
   eşlemesinden R-X, pencereden RW-NX. Yani W^X sanal sayfa düzeyinde
   geçerli, fiziksel çerçeve düzeyinde değil.

   Bu bilinçli bir kabul, kaza değil. M3'te PMAP'e yazan tek şey kernel'in
   kendisi. Gerçek tehdit M8'de user kodu eşleme yaptırabildiğinde
   başlıyor, ve o milestone'dan önce kapatılacak: PMAP kurulurken imajın
   fiziksel aralığı salt okunur eşlenecek. Ayrıntı ve bedel
   [ADR 010](adr/010-hhdm-kaderi.md), "PMAP, kernel .text'i yazılabilir
   olarak takma adlıyor".

   *Zorlayan:* `paging::map`'in bayrak doğrulaması; `check_kernel.py`
   `.text`'in ELF'te bile yazılabilir işaretlenmediğini denetliyor;
   `vm::verify_before_switch` geçiş öncesi `.text`'in RX ve yazılamaz
   olduğunu doğruluyor. *Zorlanmayan:* takma ad — bu yüzden istisna olarak
   yazılı.

5. **Her kernel stack'inin altında eşlenmemiş bir sayfa vardır.** Taşma #PF
   üretir, sessiz bozulma değil.
   *Zorlayan:* `layout::kstack_slot` her slotu bir guard sayfasıyla
   başlatıyor; `vm-guard-page` senaryosu IST yığınının altına yazıp #PF
   alıyor; `vm-stack-overflow` senaryosu ana yığından gerçekten taşıp
   aynı sonuca ulaşıyor. İkincisi #DF'e yükseliyor — #PF'i teslim etmek
   için gereken yığın da tükenmiş durumda — ve IST1 devreye giriyor, ki
   M1'in bu tasarımının ilk gerçek (imal edilmemiş) sınavı.

6. **Heap'in iki ucunda da eşlenmemiş birer sayfa vardır** (M4). Alttaki
   işaretçi alt taşmasını, üstteki sınırın aşılmasını yakalar. İkisi de
   hiçbir zaman eşlenmiyor, yani koruma heap'in ne kadar büyüdüğünden
   bağımsız.
   *Zorlayan:* `heap-guard` senaryosu üst guard'a yazıp #PF alıyor
   (CR2 = `0xffffc00004001000`); `heap::grow` `KHEAP_END`'i geçmeyi
   reddediyor ve `heap-oom` senaryosu bunun tuttuğunu gösteriyor.

## 5 seviyeli sayfalama (LA57)

Bu harita 4 seviyeli sayfalama (48-bit) varsayıyor. 5 seviyeli sayfalama
57-bit sanal adres verir ve kanonik boşluğu kaydırır; `0xFFFF_8000_...`
adresi LA57 altında da kanoniktir, ama kernel yarısının başlangıcı
`0xFF00_0000_0000_0000`'a iner.

**Karar: M3'te LA57 kullanılmayacak.** Limine'in paging mode isteği 4
seviyeyi talep edecek. Gerekçe ve ileride açmanın maliyeti
[ADR 009](adr/009-adres-uzayi-yerlesimi.md)'da.

## Bir bileşen kendi adres uzayını nasıl kurar (M11)

M7 bir adres uzayını capability'lerden kurdu, kernel içinden. M11
aynı şeyi CPL 3'ten yapıyor. Kernel tarafında tek bir yeni satır
mantık yok — M7'nin fonksiyonları baştan capability alacak şekilde
yazıldığı için ([ADR 011](adr/011-sayfa-tablosu-api.md)'in mekanizma
/ politika ayrımının ödülü) — ama **çözümlemenin nereden
başladığı** değişti.

### Her thread kendi CSpace'inden çözüyor

M8'den M10'a kadar dispatcher her CPtr'yi init CSpace'inde
çözüyordu. İki bileşen olduğu anda bu bir izolasyon deliği: tek
CSpace, her thread'in her capability'yi adlandırabilmesi demek.
M11'den itibaren her TCB'nin kendi kökü var
([ADR 043](adr/043-user-space-vspace-yonetimi.md)):

```
Tcb {
    cspace_root: u64,       // kök CNode'un fiziksel adresi, ya da 0
    cspace_slot: u64,       // capability'nin bulunduğu slotun adresi
    cspace_generation: u8,  // o slotun generation'ı (ADR 018)
    cspace_radix: u8,
}
```

Sıfır init CSpace'i demek — bir geçici çözüm değil, bir tanım: boot
thread'i init'in kendisi. `vspace_root` üçlüsüyle birebir aynı
şekil, ve aynı sebeple: ikisi de "bu thread şu nesneye bağlı, ve o
nesne hâlâ aynı nesne" diyor.

### Bir CPtr başka bir CSpace'te ne demek?

**Hiçbir şey demek değil diye bir kural yok.** Orada ne varsa onu
adlandırır.

Bu, M11'in en pahalı dersi oldu. `vspace-user-foreign`'ın ilk hâli
B'ye A'nın VSpace CPtr'sini verdi ve B başardı — çünkü iki
CSpace'in şekli aynıydı ve `0x1` her ikisinde de "kendi leaf'imin
1. slotu" demekti. İzolasyon çalışıyordu; test yanlıştı.

İzolasyonu göstermek asimetriyi **kurmayı** gerektiriyor: A'nın
VSpace'i için B'nin hiç kullanmadığı bir indekste ikinci bir
capability. B'nin oradaki slotu boş, sonuç `EmptySlot` — yasak
değil, **yok**.

### Bileşenin yapabildikleri

Dokuz invocation, ve hepsi M7'nin fonksiyonlarını değişmeden
çağırıyor:

| invocation | hedef | ne yapar |
|---|---|---|
| `Untyped.Retype` | Untyped | kendi bütçesinden nesne üretir |
| `PageTable.CreateVSpace` | PageTable | taze bir kökü adres uzayı yapar |
| `PageTable.Map` | PageTable | ara tablo yerleştirir |
| `PageTable.Unmap` | VSpace | ara tabloyu geri alır |
| `Frame.Map` | Frame | sayfa eşler (izinlerle) |
| `Frame.Unmap` | VSpace | eşlemeyi kaldırır |
| `TCB.SetUserRegisters` | TCB | ring 3 giriş noktası + yığın |
| `TCB.SetSpace` | TCB | VSpace **ve** CSpace, tek çağrıda |
| `TCB.Resume` | TCB | çalıştırır |

`Unmap`'lerin hedefi nesne değil **adres uzayı**: aynı Frame birden
çok uzaya eşlenebiliyor, capability hangisinden çıkarılacağını
söylemiyor.

### Kernel yığını: bileşen ödüyor, kernel yerleştiriyor

Kernele giren bir thread'in kernel yarısında bir yığına ihtiyacı
var. Kernel-yarısı belleğini hiçbir capability adlandırmıyor — K1
öbür taraftan — yani bileşen adresi veremez, vermemeli.

Verdiği **frame'ler**. `SetUserRegisters` dört Frame'i bir CNode'da
alıyor, kernel sanal aralığı seçip haritalıyor. Bellek üzerindeki
yetki bileşende, adres uzayı üzerindeki yetki kernelde.

KSTACK bölgesi sonlu, ve tükenmesi `NoKernelStack` (hata 30) —
**`NotEnoughMemory` değil**. İkincisi "senin bütçen bitti" demek,
birincisi "paylaşılan bir havuz bitti": farklı iddialar, ve
`vspace-user-exhaust` tam olarak hangisi olduğunu kontrol ediyor.

### Bir bileşen çocuğunun kodunu nasıl yüklüyor

Çocuğun kod frame'i ebeveynin Untyped'ından yeni çıktı; kernel
oraya bir şey yazmış olamaz. **Yükleyici ebeveyn olmak zorunda** —
gerçek bir yükleyicinin yaptığı da bu, ve M12 bunu genelleştirecek.

```
1. frame'i EBEVEYNİN uzayına yazılabilir eşle
2. çocuğun komutlarını yaz
3. eşlemeyi kaldır
4. ÇOCUĞUN uzayına çalıştırılabilir eşle
```

Hiçbir adımda, hiçbir uzayda ikisi birden değil — ve ebeveyn
dikkatli olduğu için değil: yazılabilir-ve-çalıştırılabilir diye
bir kodlama yok ([ADR 032](adr/032-w-xor-x.md)). Bir bileşen
çocuğu için W+X sayfa kuramıyor, isteseler bile.