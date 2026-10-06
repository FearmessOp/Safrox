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
    Kernel heap. M4'te `alloc` buraya kurulacak.
    M3'te eşlenmez; yalnızca aralık rezerve edilir.
    Sahip: kernel                                       Dolacak: M4

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
| `KHEAP_BASE` | `0xFFFF_C000_0000_0000` | Kernel heap tabanı |
| `KSTACK_BASE` | `0xFFFF_C800_0000_0000` | Kernel stack alanı tabanı |
| `KMMIO_BASE` | `0xFFFF_D000_0000_0000` | MMIO alanı tabanı |
| `KIMAGE_BASE` | `0xFFFF_FFFF_8000_0000` | Kernel imajı tabanı (linker.ld) |

## Değişmezler

Bunlar M3'te `debug_assert!` ve link sonrası denetimle zorlanır:

1. **Hiçbir user adresi `KERNEL_MIN`'in üstünde olamaz.** Tersi de geçerli:
   hiçbir kernel aralığı `USER_MAX`'ın altına inmez. Sınır tek bir
   karşılaştırmayla kontrol edilebilir.
2. **PMAP yalnızca RAM içerir.** MMIO ve framebuffer PMAP'e girmez; KMMIO'ya
   eşlenir. Bir cihaz penceresini PMAP üzerinden okumak mümkün olmamalı.
3. **PMAP sayfaları NX'tir.** Veri olarak eşlenen hiçbir şey yürütülemez.
4. **Kernel imajı dışındaki hiçbir kernel eşlemesi hem yazılabilir hem
   yürütülebilir değildir** (W^X).
5. **Her kernel stack'inin altında eşlenmemiş bir sayfa vardır.** Taşma #PF
   üretir, sessiz bozulma değil.

## 5 seviyeli sayfalama (LA57)

Bu harita 4 seviyeli sayfalama (48-bit) varsayıyor. 5 seviyeli sayfalama
57-bit sanal adres verir ve kanonik boşluğu kaydırır; `0xFFFF_8000_...`
adresi LA57 altında da kanoniktir, ama kernel yarısının başlangıcı
`0xFF00_0000_0000_0000`'a iner.

**Karar: M3'te LA57 kullanılmayacak.** Limine'in paging mode isteği 4
seviyeyi talep edecek. Gerekçe ve ileride açmanın maliyeti
[ADR 009](adr/009-adres-uzayi-yerlesimi.md)'da.