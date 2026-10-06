# PythOS nesne modeli

Status: fixed at M5 (architect pass). Bu belgedeki **nesne kümesi, CPtr
formatı ve hak bitleri kalıcıdır**; M13'e kadar değişmeyecek. Gerekçeler
[ADR 017](adr/017-nesne-kumesi.md) – [ADR 023](adr/023-test-disiplini.md)
arasında.

Kaynak: seL4 Reference Manual v13. Ayrıldığımız noktalar her bölümde
ayrıca işaretli.

---

## 1. Temel kavramlar

**Capability.** Bir nesneye erişim hakkı. Kopyalanabilir, hakları
daraltılarak devredilebilir, geri alınabilir. Bir CNode slot'unda durur.

**Slot.** 32 bayt. İçeriği:

```
+------------------------+------------------------+
| object: 8 B            | type(5) rights(4)      |
|   nesnenin fiziksel    | depth(3) badge/size    |
|   adresi               |              : 8 B     |
+------------------------+------------------------+
| mdb_prev: 8 B          | mdb_next: 8 B          |
|   derivation listesi, preorder (ADR 019)        |
+------------------------+------------------------+
```

Boş slot: `object == 0`.

**CSpace.** Bir bileşenin sahip olduğu tüm cap'ler. Kök bir CNode ve
(isteğe bağlı) ikinci seviye CNode'lardan oluşur.

**CPtr.** 64 bit. CSpace'te bir slot'u adresler.

```
 63                    32 31           16 15            0
+------------------------+---------------+---------------+
|  rezerve — sıfır olmalı |   L1 indeks   |   L0 indeks   |
+------------------------+---------------+---------------+
```

Çözümleme döngüsüz: iki bellek erişimi, üç karşılaştırma, ≤ 20 komut.
Tek seviyeli CSpace'te L0 = 0 ([ADR 018](adr/018-cptr-ve-cspace.md)).

---

## 2. Nesneler

Boyutlar M5 uygulamasında ölçülüp güncellenecek; aşağıdakiler tavan.

### Untyped

| | |
|---|---|
| Boyut | 2^k bayt, k ∈ [4, 47] |
| Hizalama | 2^k |
| Değişmez | `watermark ∈ [base, base + size]`, asla azalmaz |
| Değişmez | Çocukların tamamı `[base, watermark)` içinde |

Fiziksel belleğin capability'si. Her nesne bir Untyped'dan retype
edilerek doğar.

**Invocation'lar:**

| İşlem | Hak | Etki |
|---|---|---|
| `retype(type, size_bits, count, dest, offset)` | `WRITE` | `count` nesne üretir, cap'lerini `dest[offset..]`'a yazar |

`retype` atomiktir ve `count ≤ 256`
([ADR 020](adr/020-retype-semantigi.md)).

### CNode

| | |
|---|---|
| Boyut | 32 × 2^radix bayt, radix ∈ [1, 16] |
| Hizalama | nesne boyutu |
| Değişmez | Her slot ya boş ya geçerli bir cap |
| Değişmez | Bir CNode kendi içinde döngü oluşturamaz (kontrol: retype derinliği) |

**Invocation'lar:**

| İşlem | Hak | Etki |
|---|---|---|
| `copy(src, dest, rights)` | src: `READ`, dest: `WRITE` | Cap'i kopyalar, hakları daraltır |
| `mint(src, dest, rights, badge)` | src: `READ|GRANT`, dest: `WRITE` | Kopyalar + badge takar |
| `move(src, dest)` | src: `READ|WRITE`, dest: `WRITE` | Taşır, kaynağı boşaltır |
| `delete(slot)` | `WRITE` | Tek cap'i siler |
| `revoke(slot)` | `WRITE` | **Torunları** siler, slot'u bırakır |

`copy` hakları genişletemez ([ADR 021](adr/021-capability-haklari.md)).

### TCB

| | |
|---|---|
| Boyut | 1024 B |
| Hizalama | 1024 B |
| Değişmez | `cspace_root` geçerli bir CNode cap'i ya da boş |
| Değişmez | `vspace_root` geçerli bir PageTable cap'i ya da boş |
| Değişmez | Aynı anda en fazla bir kuyrukta (hazır / bekleyen / durmuş) |

**Invocation'lar:**

| İşlem | Hak | Etki |
|---|---|---|
| `configure(cspace, vspace, ipc_buffer)` | `WRITE` | Kök cap'leri ve IPC tamponunu bağlar |
| `write_registers(regs)` | `WRITE` | Kayıtları yazar |
| `read_registers()` | `READ` | Kayıtları okur |
| `resume()` | `WRITE` | Hazır kuyruğuna koyar |
| `suspend()` | `WRITE` | Durdurur |
| `set_priority(p)` | `WRITE` | Öncelik (M10) |

### Endpoint

| | |
|---|---|
| Boyut | 32 B |
| Hizalama | 32 B |
| Değişmez | Kuyrukta ya yalnızca gönderenler ya yalnızca alıcılar var, ikisi birden asla |

Senkron IPC randevu noktası. Mesaj Endpoint'te değil, gönderenin
TCB'sinde durur.

**Invocation'lar:**

| İşlem | Hak | Etki |
|---|---|---|
| `send(msg)` | `WRITE` | Alıcı gelene kadar bloke |
| `nbsend(msg)` | `WRITE` | Alıcı yoksa mesajı düşürür |
| `recv()` | `READ` | Gönderen gelene kadar bloke |
| `call(msg)` | `WRITE` | Gönder + cevap bekle |
| `reply(msg)` | örtük | Son `call`'a cevap ver |

Cap taşımak `GRANT` gerektirir; yoksa cap alanları **sessizce boş
kalır**.

### Notification

| | |
|---|---|
| Boyut | 48 B |
| Hizalama | 64 B |
| Değişmez | `signals` bir bit maskesi; `wait` onu okuyup sıfırlar |

Asenkron sinyal. Gönderen bloke olmaz; sinyaller birikir.

**Invocation'lar:**

| İşlem | Hak | Etki |
|---|---|---|
| `signal()` | `WRITE` | Badge bitlerini maskeye OR'lar, bekleyeni uyandırır |
| `wait()` | `READ` | Maske sıfırsa bloke; değilse maskeyi döndürüp sıfırlar |
| `poll()` | `READ` | Bloke olmadan maskeyi okur ve sıfırlar |

### Frame

| | |
|---|---|
| Boyut | 4096 B |
| Hizalama | 4096 B |
| Değişmez | **En fazla bir yere eşli** (M11'e kadar) |

**Invocation'lar:**

| İşlem | Hak | Etki |
|---|---|---|
| `map(vspace, vaddr, flags)` | `READ` ve/veya `WRITE` | Eşler; `flags` cap haklarını aşamaz |
| `unmap()` | `WRITE` | Eşlemeyi kaldırır |

Eşleme izinleri cap haklarıyla **kesişir**: `READ`-only bir cap,
yazılabilir eşlenemez.

### PageTable

| | |
|---|---|
| Boyut | 4096 B |
| Hizalama | 4096 B |
| Değişmez | Seviye, eşlendiği yerde belli — nesnede saklı değil |
| Değişmez | Bir tablo en fazla bir üst tabloya eşli |

Tek tip, dört seviye ([ADR 017](adr/017-nesne-kumesi.md)).

**Invocation'lar:**

| İşlem | Hak | Etki |
|---|---|---|
| `map(parent, vaddr)` | `WRITE` | Üst tabloya bir seviye altına eşler |
| `unmap()` | `WRITE` | Üstten kaldırır |

### IRQHandler

| | |
|---|---|
| Boyut | 32 B |
| Hizalama | 32 B |
| Değişmez | Bir IRQ hattına en fazla bir IRQHandler |

**Invocation'lar:**

| İşlem | Hak | Etki |
|---|---|---|
| `set_notification(ntfn)` | `WRITE` | Kesmeyi bu Notification'a yönlendirir |
| `ack()` | `WRITE` | Kesmeyi onaylar, hattı tekrar açar |
| `clear()` | `WRITE` | Yönlendirmeyi kaldırır, hattı maskeler |

---

## 3. Haklar

| Bit | Ad | Endpoint | Notification | Frame | CNode | Untyped | TCB | PageTable | IRQHandler |
|---|---|---|---|---|---|---|---|---|---|
| 0 | `READ` | recv | wait | okunabilir eşle | copy kaynağı | — | durum oku | — | — |
| 1 | `WRITE` | send | signal | yazılabilir eşle | copy hedefi | retype | başlat/durdur | eşle/çöz | bağla/ack |
| 2 | `GRANT` | cap taşı | — | — | — | — | — | — | — |
| 3 | `GRANTREPLY` | cevap cap'i | — | — | — | — | — | — | — |

Boş hücre: o hakkın o nesnede etkisi yok. Hata değil, etkisiz.

**Monoton daralma:** bir cap'in hakları, türediği cap'in haklarının alt
kümesidir. İstisnasız, ve tümevarımla korunuyor — çalışma zamanında ağaç
yürüyüşü yok.

---

## 4. Bir Untyped'dan Endpoint nasıl doğar

```
BAŞLANGIÇ — init bileşeninin CSpace'i

  root CNode (radix 8, 256 slot)
  +--------+--------+--------+--------+-----
  | slot 0 | slot 1 | slot 2 | slot 3 | ...
  | UT-A   | (boş)  | (boş)  | (boş)  |
  +--------+--------+--------+--------+-----
      |
      v
  Untyped A: base=0x200000, size=2^20 (1 MiB), watermark=0x200000

  derivation listesi:  [UT-A]
                        d=0


ADIM 1 — retype(UT-A, Endpoint, count=2, dest=root, offset=1)

  faz 1: doğrula — slot 1 ve 2 boş mu, 2x32 B sığıyor mu, derinlik <= 8 mi
  faz 2: 64 bayt sıfırla, iki Endpoint başlat, cap'leri yaz

  root CNode
  +--------+--------+--------+--------+-----
  | slot 0 | slot 1 | slot 2 | slot 3 | ...
  | UT-A   | EP-1   | EP-2   | (boş)  |
  +--------+--------+--------+--------+-----
      |        |        |
      |        v        v
      |    0x200000  0x200020     <- UT-A'nın içinden kesildi
      v
  Untyped A: watermark=0x200040   <- 64 bayt ilerledi

  derivation listesi:  [UT-A] -> [EP-1] -> [EP-2]
                        d=0      d=1      d=1


ADIM 2 — mint(EP-1, dest=slot 3, rights=WRITE, badge=0x42)

  Bir istemciye "yalnızca gönderebilirsin" cap'i veriliyor.

  root CNode
  +--------+--------+--------+--------+-----
  | slot 0 | slot 1 | slot 2 | slot 3 | ...
  | UT-A   | EP-1   | EP-2   | EP-1'  |   EP-1' : aynı nesne,
  +--------+--------+--------+--------+---         rights=WRITE,
      |        |        |        |                 badge=0x42
      |        v        v        |
      |    0x200000  0x200020 <--+   iki cap, tek nesne
      v
  Untyped A: watermark=0x200040   <- DEĞİŞMEDİ, yeni nesne yok

  derivation listesi:  [UT-A] -> [EP-1] -> [EP-1'] -> [EP-2]
                        d=0      d=1       d=2        d=1
                                            ^
                                   EP-1'in çocuğu, hemen ardında


ADIM 3a — revoke(slot 1)   "EP-1'den verdiklerimi geri al"

  Listede EP-1'den ileri tara, depth > 1 olduğu sürece sil.
  EP-1' (d=2) silinir. EP-2 (d=1) silinmez — torun değil, kardeş.

  root CNode
  +--------+--------+--------+--------+-----
  | slot 0 | slot 1 | slot 2 | slot 3 |
  | UT-A   | EP-1   | EP-2   | (boş)  |   <- EP-1' gitti
  +--------+--------+--------+--------+-----

  derivation listesi:  [UT-A] -> [EP-1] -> [EP-2]

  Endpoint nesnesi 0x200000 HÂLÂ VAR: EP-1 onu hâlâ gösteriyor.
  Untyped A'nın watermark'ı DEĞİŞMEDİ.


ADIM 3b — revoke(slot 0)   "UT-A'dan verdiklerimi geri al"

  Listede UT-A'dan ileri tara, depth > 0 olduğu sürece sil.
  EP-1, EP-1', EP-2 — hepsi silinir.

  Her cap silinirken: bu nesnenin son cap'i mi?
    EP-1' silinir -> hayır, EP-1 kaldı -> nesne durur
    EP-1  silinir -> evet -> Endpoint yok edilir
                             (bekleyen thread'ler hata ile uyandırılır)
    EP-2  silinir -> evet -> Endpoint yok edilir

  root CNode
  +--------+--------+--------+--------+-----
  | slot 0 | slot 1 | slot 2 | slot 3 |
  | UT-A   | (boş)  | (boş)  | (boş)  |
  +--------+--------+--------+--------+-----
      |
      v
  Untyped A: watermark=0x200000  <- SIFIRLANDI, 1 MiB tekrar kullanılabilir

  derivation listesi:  [UT-A]

  UT-A cap'inin kendisi duruyor: revoke torunları siler, kökü bırakır.
  Tamamen yok etmek icin: revoke(slot 0) sonra delete(slot 0).
```

**Diyagramın söylediği üç şey:**

1. `mint` watermark'ı ilerletmez — yeni nesne yok, yeni cap var.
2. `revoke` kardeşlere dokunmaz; preorder listesi ve `depth` bunu
   ayırıyor.
3. Bellek ancak **tüm** cap'ler gittiğinde geri döner, ve Untyped'ın
   watermark'ı ancak revoke ile sıfırlanır.

---

## 5. Değişmezler

Bunlar M5 uygulamasında `assert!` ve kabul testleriyle zorlanacak.

1. **Bir slot ya boştur ya geçerli bir cap taşır.** Yarı yazılmış slot
   yok — retype atomik ([ADR 020](adr/020-retype-semantigi.md)).

2. **Derivation listesi preorder sırada.** Bir cap'in torunları, listede
   hemen ardından ve kesintisiz gelir. Revoke bu özelliğe dayanıyor.

3. **Derinlik ≤ 8.** `copy` ve `retype`, aşacaksa `DerivationTooDeep`
   döner ([ADR 019](adr/019-derivation-tree.md)).

4. **Haklar monoton daralır.** Hiçbir cap, atasının haklarından fazlasına
   sahip olamaz ([ADR 021](adr/021-capability-haklari.md)).

5. **Untyped'ın watermark'ı asla azalmaz** — revoke hariç, ki o da
   başa döndürür.

6. **Retype edilen bellek her zaman sıfırlanır.** İstisnasız, çünkü
   retype belleğin user'a ulaştığı tek yol.

7. **Bir nesnenin son cap'i silinince nesne yok edilir** ve belleği
   Untyped'a döner.

8. **CPtr'ın üst 32 biti sıfırdır.** Değilse `InvalidCPtr`
   ([ADR 018](adr/018-cptr-ve-cspace.md)).

---

## 6. Hata kodları

Bir invocation başarısız olduğunda dönen kodlar. Hepsi kerneli
durdurmadan geri döner — user'ın yanlış isteği kernel hatası değildir.

| Kod | Anlamı |
|---|---|
| `InvalidCPtr` | CPtr formatı bozuk (üst bitler sıfır değil) |
| `CPtrOutOfRange` | CNode sınırının dışında |
| `EmptySlot` | Slot boş |
| `WrongType` | Cap bu invocation'ı desteklemiyor |
| `InsufficientRights` | Gerekli hak bit'i yok |
| `RightsEscalation` | `copy` kaynaktan fazla hak istedi |
| `SlotsNotEmpty` | Hedef slot dolu |
| `DerivationTooDeep` | Derinlik 8'i aşıyor |
| `NotEnoughMemory` | Untyped'ın kalanı yetmiyor (kalan bayt da döner) |
| `TooManyObjects` | `count > 256` |
| `AlreadyMapped` | Frame/PageTable zaten eşli |
| `NotMapped` | Eşli değil |
| `InvalidArgument` | Diğer (boyut, hizalama, tip kombinasyonu) |

---

## 7. M13'e kadar bilinçli olarak yapılmayanlar

| Eksik | Bedeli | Nerede |
|---|---|---|
| İkiden derin CSpace | Çok kademeli delege yok | [ADR 018](adr/018-cptr-ve-cspace.md) |
| Nesil sayacı | Slot yeniden kullanımında ABA riski | [ADR 018](adr/018-cptr-ve-cspace.md), soru 2 |
| Bölünebilir revoke | En kötü 56 ms thread değişimi yok | [ADR 019](adr/019-derivation-tree.md) |
| Paylaşımlı bellek | Frame tek yere eşli | [ADR 020](adr/020-retype-semantigi.md), soru 4 |
| PCID | Her CR3 yüklemesi tam TLB flush | [ADR 017](adr/017-nesne-kumesi.md) |
| Bütçeli zamanlama | Gerçek zamanlı garanti yok | [ADR 017](adr/017-nesne-kumesi.md) |
| Devredilebilir Reply | Cevap hakkı başkasına verilemez | [ADR 017](adr/017-nesne-kumesi.md) |
| Heap'in Untyped'ı | Kernel'in bütçesiz rezervi var | [ADR 022](adr/022-kernelin-ayricalikli-kaynaklari.md) |