# M5 architect pass — karara bağlanamayanlar

Aşağıdaki altı nokta ya sizin kararınızı gerektiriyor ya da tek başıma
verdiğim kararın gözden geçirilmesini istiyorum. Her biri için seçenekler
ve benim eğilimim yazılı; M5 uygulamasına başlamadan önce cevaplanmalı,
çünkü hepsi veri yapılarını etkiliyor.

Numaralar ADR'lerden referans veriliyor.

---

## Soru 1 — Revoke: 56 ms kabul edilebilir mi?

**Durum:** [ADR 019](adr/019-derivation-tree.md). Revoke'un en kötü
durumu 256 MiB'lık bir makinede **~56 ms**, ve o süre boyunca hiçbir
thread çalışmıyor (kesmeler açık, thread değişimi yok).

Pratikte M13'e kadar mikrosaniyeler olacak. Ama bir bileşen kasten geniş
bir derivation ağacı kurup bu süreyi uzatabilir — bir DoS vektörü.

**Seçenekler:**

**(a) Kabul et, M10'da ölç.** Benim seçtiğim. Tutarlılık basit kalıyor,
ve zamanlayıcı gelmeden latency'yi ölçmek anlamsız.

**(b) Nesil (generation) sayacı ile O(1) revoke.** Revoke, atanın nesil
numarasını artırır; torunlar bir anda geçersizleşir. Bedeli: her CPtr
çözümlemesinde atalar zincirinin nesillerini doğrulamak — 8 okuma, ve
[ADR 018](adr/018-cptr-ve-cspace.md)'in 20 komutluk hedefi ~60'a çıkar.
**Her syscall'ı pahalılaştırıp nadir bir işlemi ucuzlatıyor.**

**(c) Bölünebilir revoke.** `revoke` en fazla N slot siler ve
"tamamlanmadı" döner. Yetki, çağıran döngüyü tamamlayana kadar dışarıda
kalır — bence bu revoke'un anlamını bozuyor.

**Sorum:** (a) ile devam edeyim mi? Eğer gerçek zamanlı davranış bu
projenin hedeflerinden biriyse (b)'yi şimdi yapmak gerekir, çünkü
sonradan eklemek CPtr formatını ve her lookup'ı değiştirir.

---

## Soru 2 — Slot yeniden kullanımı: ABA riski

**Durum:** [ADR 018](adr/018-cptr-ve-cspace.md). Bir slot silinip
yeniden doldurulduğunda, eski CPtr'ı elinde tutan bir bileşen **yeni
nesneyi** gösterir. Kernel bunu fark edemez.

Senaryo: A bileşeni B'ye "slot 5'teki Endpoint'e yaz" der. B yazmadan
önce slot 5 silinir ve yerine bir Untyped konur. B'nin yazması artık
Untyped'a gider.

Pratikte: cap'ler CPtr olarak değil, cap olarak devrediliyor (IPC ile
taşınıyor), yani bu senaryo için A'nın B'ye bir *sayı* vermesi gerekir.
Bu bir protokol hatası sayılabilir.

Ama M13'ün init sistemi muhtemelen "şu CPtr'da şu cap var" diye bir
tablo paylaşacak ve o tablo bayatlayabilir.

**Seçenekler:**

**(a) Yok say, protokol sorunu de.** Benim eğilimim, ama rahat değilim.

**(b) Nesil sayacı (rezerve bitlerde).** CPtr'ın üst 32 bitinin bir
kısmı slot'un nesil numarası olur; çözümlemede karşılaştırılır. Maliyet:
slot başına birkaç bit ve çözümlemede **bir** karşılaştırma — soru
1'deki (b)'den çok daha ucuz, çünkü atalar zincirini yürümüyor.

**Sorum:** (b) ucuz ve rezerve alan zaten var. Şimdi mi yapalım,
yoksa M13'te ihtiyaç görülürse mi? Şimdi yapmanın maliyeti bir
karşılaştırma; sonra yapmanın maliyeti CPtr üreten her kodu
değiştirmek.

---

## Soru 3 — MMIO'yu ne temsil ediyor?

**Durum:** [ADR 017](adr/017-nesne-kumesi.md). M12'de bir cihaz
sürücüsü register penceresine erişecek. Şimdilik cevap "bir Frame, ama
`NO_CACHE` ile eşlenen".

Problem: bir Frame bir Untyped'dan retype ediliyor, ve Untyped'lar RAM'i
temsil ediyor. Cihaz register'ları RAM değil. İki soru doğuyor:

- Bir bileşen rastgele bir fiziksel adresi Frame'e çevirebilmeli mi?
  **Hayır** — o zaman herhangi bir cihazı ele geçirebilir.
- O halde MMIO Frame'leri nereden geliyor?

**Seçenekler:**

**(a) Boot'ta kernel, MMIO bölgeleri için ayrı "device Untyped"lar
üretir** ve init'e verir. Bunlar normal Untyped gibi retype edilir ama
yalnızca Frame'e, ve üretilen Frame'ler `NO_CACHE` ile eşlenir. Yeni tip
yok, bir bayrak var.

**(b) Ayrı bir `DeviceFrame` nesne tipi.** Dokuzuncu tip. Daha açık ama
nesne kümesini büyütüyor.

**(c) M12'ye ertele.** Şimdi karar vermemek, M12'de nesne modelini
değiştirmek zorunda kalma riski.

**Benim eğilimim (a).** Ama MMIO bölgelerinin memory map'ten nasıl
çıkarılacağı (ACPI gerekiyor mu?) M6/M7'de netleşecek.

**Sorum:** (a) yeterli mi, yoksa M12'de bir şeyi kaçırdığımı mı
göreceğim?

---

## Soru 4 — Paylaşımlı bellek M11'e kadar yok

**Durum:** [ADR 020](adr/020-retype-semantigi.md). Bir Frame en fazla
bir yere eşlenebilir. Sebep: revoke sırasında bir Frame'in tüm
eşlemelerini bulmak, Frame başına bir eşleme listesi tutmayı gerektirir.

Sonuç: **iki bileşen aynı belleği paylaşamaz.** IPC ile veri kopyalamak
zorundalar.

Bu, M9'un IPC tasarımını etkiliyor: büyük veri transferi için
paylaşımlı bellek yoksa, her transfer bir kopya. Küçük mesajlar için
sorun değil (kayıtlarda taşınıyor), büyükler için ciddi.

**Seçenekler:**

**(a) Kabul et, M11'de ekle.** Frame'e bir eşleme listesi eklemek,
Frame nesnesinin boyutunu değiştirir ama tip kümesini değiştirmez.

**(b) Şimdi ekle.** Frame 4096 bayt zaten; eşleme listesi için nesnenin
*yanında* bir yapı gerekir. Karmaşıklık, M11'e kadar kullanılmayacak
bir şey için.

**(c) Sabit küçük bir liste (örneğin 4 eşleme).** Orta yol. "4 neden?"
sorusunu doğuruyor.

**Benim eğilimim (a).** Ama M9'un IPC'sini tasarlarken "paylaşımlı
bellek yok" kısıtı bir yere sıkıştırırsa geri dönmek gerekebilir.

**Sorum:** M9'da büyük veri transferi bir gereksinim mi? Eğer M13'ün
init sistemi bir dosya sistemi ya da ağ yığını içerecekse evet; yalnızca
birkaç küçük servis ise hayır.

---

## Soru 5 — TCB 1024 bayt: IPC tamponu nerede?

**Durum:** [ADR 017](adr/017-nesne-kumesi.md). TCB 1024 bayt olarak
tahmin edildi: kayıtlar + `fxsave` alanı (512 B) + zamanlayıcı durumu +
kök cap'ler.

seL4'te **IPC buffer** ayrı bir Frame: thread'in adres uzayında eşli,
mesaj verisi oraya yazılıyor. Kernel o Frame'e kendi eşlemesi üzerinden
erişiyor.

Alternatif: IPC tamponunu TCB'nin içine koymak. O zaman TCB büyür (örneğin
2048 bayt) ama ayrı bir Frame ve ayrı bir eşleme gerekmez.

**Seçenekler:**

**(a) seL4 gibi, ayrı Frame.** Esnek — tampon boyutu bileşenin kararı,
ve TCB küçük kalıyor. Bedeli: her IPC'de kernel'in o Frame'e erişmesi
gerekiyor, yani ya PMAP üzerinden (fiziksel adresi bilmek) ya da kalıcı
bir kernel eşlemesi.

**(b) TCB'nin içinde sabit boyutlu tampon.** Basit ve hızlı — kernel
TCB'ye zaten erişiyor. Bedeli: tampon boyutu sabit (örneğin 1 KiB) ve
TCB iki katına çıkıyor.

**Benim eğilimim (b)**, çünkü M9'un IPC yolunu basitleştiriyor ve
"paylaşımlı bellek yok" kısıtıyla (soru 4) tutarlı: zaten küçük mesajlar
taşıyacağız.

**Sorum:** Bu, M9'u doğrudan etkileyen bir karar. (b) ile gidersem IPC
mesaj boyutu ~1 KiB ile sınırlanıyor. Yeterli mi?

---

## Soru 6 — Hangi invocation mekanizması?

**Durum:** Hiçbir ADR'de karara bağlanmadı, çünkü M8'in konusu. Ama
nesne modelini etkiliyor, bu yüzden şimdi soruyorum.

Bir cap'i "invoke" etmek bir syscall. Argümanlar nasıl geçiyor?

**(a) Tek bir `invoke` syscall'ı, ilk argüman CPtr, ikinci argüman
"hangi işlem".** seL4'ün yaklaşımı. Syscall tablosu küçük, işlem
numaraları nesne tipine göre yorumlanıyor.

**(b) İşlem başına syscall.** `sys_retype`, `sys_copy`, `sys_send`...
Daha açık, daha kolay hata ayıklanır. Syscall tablosu büyüyor ama o bir
dizi indeksi.

**Benim eğilimim (a)**, çünkü nesne modeli büyüdükçe (b) yönetilemez
hale gelir ve ikisi arasındaki performans farkı bir dal.

**Sorum:** M8'de karar vereceğim ama şimdi bir tercihiniz varsa,
invocation tablosunu ona göre tasarlarım.

---

## Karara bağlanmış ve sormadıklarım

Aşağıdakiler tek başıma karara bağlandı; itiraz ederseniz söyleyin.

- **Sekiz nesne tipi**, PCID ve bütçeli zamanlama dahil değil
  ([ADR 017](adr/017-nesne-kumesi.md)).
- **PageTable tek tip**, dört seviye yerine
  ([ADR 017](adr/017-nesne-kumesi.md)).
- **CPtr iki seviye, guard yok** — seL4'ten en büyük ayrılma
  ([ADR 018](adr/018-cptr-ve-cspace.md)).
- **Derivation derinliği 8** ([ADR 019](adr/019-derivation-tree.md)).
- **`count ≤ 256`** retype başına ([ADR 020](adr/020-retype-semantigi.md)).
- **Her retype'ta sıfırlama**, istisnasız
  ([ADR 020](adr/020-retype-semantigi.md)).
- **Dört hak biti**, seL4 ile aynı
  ([ADR 021](adr/021-capability-haklari.md)).
- **GRANT yokken cap sessizce düşürülüyor**, hata dönmüyor
  ([ADR 021](adr/021-capability-haklari.md)) — bu tartışmalı olabilir.
- **Kernel heap'i bütçesiz bir rezerv**, 64 KiB
  ([ADR 022](adr/022-kernelin-ayricalikli-kaynaklari.md)).
- **`frame_allocator` boot'tan sonra emekli**
  ([ADR 022](adr/022-kernelin-ayricalikli-kaynaklari.md)).