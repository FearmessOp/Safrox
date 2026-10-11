# ADR 056 — QEMU'ya tuş enjeksiyonu

- **Durum:** Kabul edildi
- **Tarih:** M15
- **Bağlam:** ADR 023 (kabul testi disiplini), ADR 053 (klavye)
- **Referans:** QEMU QMP referansı, `send-key` komutu; `qapi/ui.json`

## Bağlam

Klavye bileşeninin test edilmesi, teste bir tuş basılmasını
gerektiriyor. `run.py` şu ana kadar hiç girdi göndermedi:
QEMU'yu başlatıyor, seri çıktıyı topluyor, çıkış kodunu
yorumluyor.

Brief bu değişikliğe izin veriyordu. Soru, ne kadarına ihtiyaç
olduğu.

## Olgu: komutun kabul edilmesi, baytın gelmesi değildir

İlk ölçüm `send-key`'in `{'return': {}}` döndürdüğünü gösterdi. Bu
**hiçbir şey kanıtlamıyor**: QMP komutun geçerli olduğunu söylüyor,
misafirin bir şey gördüğünü değil.

Kanıt için misafirin 0x60'ı okuması gerekiyordu. `scripts/m15-spike/`
bunun için yazıldı: 512 baytlık bir boot sector, 0x64'ü yokluyor,
0x60'tan okuduğu her baytı seri porta onaltılık basıyor. Hiçbir
şeye bağımlı değil — ne Limine'e, ne kernel'e, ne capability
sistemine.

```
SPIKE-READY 1e 9e 2a 1e 9e aa e0 48 e0 c8 1d 2e ae 9d 38 2d ad b8
```

Baytlar geliyor. Ayrıca scan code setinin 1 olduğu da buradan
çıktı (ADR 053) — spike, sorulmayan bir soruyu da yanıtladı.

**Spike silinmedi.** `scripts/m9-index-probe/` precedent'i:
sonradan "acaba gerçekten öyle miydi" diye sorulduğunda yeniden
yazmak yerine yeniden koşturulabilen bir şey kalıyor.

## Karar: ikinci bir yol, mevcut yola dokunmadan

`run.py`'ın bugünkü çalışması:

```python
completed = subprocess.run(cmd, stdout=PIPE, timeout=..., check=False)
```

Bloklayan, borusu sonunda toplu okunan bir çağrı. Tuşun *ne zaman*
gönderileceğini bilmek için seri çıktının **artımlı** okunması
gerekiyor — bileşen "hazırım" dediğinde göndermek için. Sabit bir
`sleep` ADR 023'ün K8'ini çiğner: olayı bekle, süreyi değil.

Yani `Popen` + okuma thread'i + QMP soketi gerekiyor. Bu, 66
senaryonun tamamının geçtiği çekirdek.

**Karar: yalnızca `keys` bildiren senaryolar yeni yoldan geçiyor.**

```python
if expect.keys:
    output, status, timed_out = run_with_keys(...)
else:
    output, status, timed_out = run_plain(...)      # değişmedi
```

`run_plain`, bugünkü `subprocess.run` çağrısının **harfi harfine
aynısı**. 65 senaryo onu kullanmaya devam ediyor, ve bir regresyon
olursa sebebi bu değişiklik olamaz.

### Neden tek bir yola birleştirmedim

Birleştirmek daha temiz görünürdü: tek kod yolu, az tekrar. Ve
yanlış olurdu.

`Popen` + thread + artımlı okuma, `subprocess.run`'dan farklı
hata modları taşıyor: borunun kapanma sırası, thread'in
birleştirilmesi, zaman aşımında yarım kalmış okuma. Bunların hepsi
66 senaryonun risk yüzeyine eklenirdi, ve kazancı yalnızca estetik.

M14'ün `print!` atomikliği hatası bu yüzden pahalıydı: iki farklı
senaryoda, farklı koşularda ortaya çıktı ve sebebi ortak bir
yoldaydı. Ortak yolu genişletmenin bedelini bir kez ödedik.

## Mekanizma

**Hazır olduğunu nasıl anlıyoruz.** Okuma thread'i seri çıktıyı
satır satır biriktiriyor; ana thread, senaryonun bildirdiği
`ready` düzenli ifadesini bekliyor. Konsol bileşeni çizmeye hazır
olduğunda bunu seri porta yazıyor.

Zaman aşımı var ve aşılması bir hata — süre beklemenin yerine
geçmiyor, yalnızca sonsuza kadar asılı kalmayı engelliyor.

**Tuşlar arası.** Her `send-key`'den sonra, o tuşun işlendiğini
bildiren satır bekleniyor. Yine olay, yine süre değil. Tuşları
peş peşe göndermek `kbd-flood` senaryosunun işi ve orada kasıtlı.

**QMP soketi.** TCP, unix soket değil: betik hem WSL'den hem
Windows'tan aynı şekilde çalışmalı. Port, çakışmayı önlemek için
senaryo başına seçiliyor ve bağlanamamak bir hata.

## Yapay scan code'a düşülmedi

Brief, enjeksiyon imkânsızsa yapay scan code üreten bir test
modunun **açıkça raporlanmasını** istiyordu.

Gerekmedi: enjeksiyon çalışıyor ve kanıtlandı. Bu, testin değerini
belirgin biçimde yükseltiyor — yapay scan code, sürücünün çözücüsünü
test ederdi ama IRQ yolunu, i8042'nin tampon davranışını ve
gecikmeyi test edemezdi. Ölçülen tuş→piksel gecikmesi, gerçek bir
kesmeden başlıyor.

Çözücü yine de **ayrıca** saf bir fonksiyon olarak test ediliyor
(ADR 053): bayt dizisi girer, olay dizisi çıkar. İkisi farklı
şeyleri kanıtlıyor ve biri diğerinin yerine geçmiyor.

## Elenen alternatifler

**(a) `-monitor` ile insan okunur protokol.** `sendkey a` yazmak
daha kısa. Reddedildi: çıktısı serbest metin ve ayrıştırması
sürüme bağlı. QMP JSON ve hata döndürdüğünde bunu söylüyor.

**(b) `-device usb-kbd` ile USB klavye.** i8042'ye hiç
dokunmazdı. Reddedildi: M15 PS/2 denetleyicisini sürmek üzerine.

**(c) QEMU'ya tuş gönderen ayrı bir betik, `run.py` dışında.**
Senkronizasyon imkânsız: betik, bileşenin hazır olduğunu
bilemezdi.

**(d) `expect` benzeri bir kütüphane.** Bağımlılık ekler;
projenin tüm test altyapısı standart kütüphaneyle yazılı.

## Sonuçlar

- QMP `send-key` çalışıyor ve **kanıtlandı**, varsayılmadı.
- `scripts/m15-spike/` kalıcı: soru yeniden sorulduğunda yeniden
  koşturulabilir.
- `run.py` iki yola ayrıldı; `keys` bildirmeyen 65 senaryo
  bugünkü kodu kullanmaya devam ediyor.
- Senkronizasyon olaya dayalı: her tuş, işlendiğini bildiren
  satır beklenerek gönderiliyor.
- Yapay scan code'a ihtiyaç olmadı; çözücü ayrıca saf fonksiyon
  olarak da test ediliyor.