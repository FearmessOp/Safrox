# ADR 052 — Lisans: MPL-2.0

- **Durum:** Kabul edildi
- **Tarih:** M14 sonrası
- **Bağlam:** [ADR 002](002-tek-binary-iki-iso.md) (tek binary),
  [ADR 043](043-user-space-vspace-yonetimi.md) (katmanlar),
  [ADR 044](044-root-task-ve-bootstrap-protokolu.md) (kernel mührü)

## Bağlam

Proje açık kaynak olacak. On dört milestone boyunca lisans sorusu
sorulmadı, ve bu bir karar değil bir **boşluktu**: kaynak dosyalarda
`SPDX-License-Identifier: BSD-2-Clause` satırı vardı, ama kökte
`LICENSE` dosyası yoktu. Satır ilk dosyanın şablonundan geldi ve
yetmiş bir dosyaya kopyalandı.

Bir SPDX satırı, lisans metni olmadan, hukuken ne verdiği belirsiz
bir iddia. Bu ADR hem boşluğu kapatıyor hem de ilk kez **bilinçli**
bir seçim yapıyor.

### Neyi koruyor olmak istiyoruz

Üç soru, ve üçünün cevabı farklı:

1. **Çekirdeği değiştiren paylaşmalı mı?** Evet. Bir mikroçekirdeğin
   bütün değeri TCB'nin küçük ve denetlenebilir olması. Kapalı bir
   fork, o denetlenebilirliği ortadan kaldırır ve projenin tek satış
   argümanını götürür.
2. **Üstüne kapalı bir şey konabilmeli mi?** Evet. Bu mimaride
   katman 3 — dosya sistemi, ağ yığını, grafik, bir personality —
   ayrı bileşenler olarak yazılacak ve **tek başına yazılamaz.** Bir
   mikroçekirdek ancak üstündeki bileşenlerle işe yarar, ve o
   bileşenleri yazacak olanların hepsi kodunu açmak zorunda
   kalmamalı.
3. **Patent riski kimde?** Katkıcıda değil, kullanıcıda da değil —
   açıkça hibe edilmeli. Bir işletim sistemi çekirdeği, patent
   iddiasının en pahalı olduğu yerlerden biri.

## Karar

**MPL-2.0** (Mozilla Public License 2.0).

Korumanın sınırı **dosya**. MPL-2.0'ın copyleft'i dosya bazında
çalışır: bu depodaki bir dosyayı değiştiren, o dosyanın kaynağını
paylaşmak zorunda; **yeni** dosyalar eklemek, hatta onları farklı
bir lisansla dağıtmak serbest.

Üç sorunun cevabı bu sınırdan çıkıyor:

- `kernel/src/cap/untyped.rs`'i değiştiren paylaşır. Çekirdek
  korunur.
- Yanına `netstack/src/main.rs` koyan paylaşmak zorunda değil.
  Katman 3 açık kalır.
- MPL-2.0 §2.1(b) açık bir patent hibesi içeriyor.

### Neden dosya, satır ya da süreç değil

Copyleft'in sınırı üç yerden biri olabilir:

| Sınır | Lisans | Sonuç |
|---|---|---|
| hiçbiri | MIT, BSD, Apache | çekirdek kapatılabilir |
| **dosya** | **MPL-2.0** | **çekirdek korunur, üstü serbest** |
| süreç / bağlantı | LGPL | bir mikroçekirdekte anlamsız |
| bütün sistem | GPL, AGPL | katman 3 de açık olmak zorunda |

LGPL satırı ayrıca teknik bir nedenle eleniyor: LGPL'in ayrımı
"dinamik bağlantı" üzerine kurulu, ve bu sistemde bileşenler
birbirine **bağlanmıyor** — `syscall` ve IPC ile konuşuyorlar
(ADR 030, ADR 033). Bağlantı kavramı yoksa, ona dayanan bir sınır da
yok.

## Elenen alternatifler

### Apache-2.0 / MIT — çekirdek kapatılabilir

İkisi de patent hibesi bakımından farklı (Apache'de var, MIT'de yok)
ama copyleft bakımından aynı: **hiç yok.** Bir şirket çekirdeği
alıp kapalı bir ürüne gömebilir, hiçbir değişikliği geri vermez.

Bu, "katkıcı hedefleniyor" hedefiyle doğrudan çelişiyor. Katkı,
katkının geri döneceği beklentisiyle yapılır. Çekirdeğin kapalı bir
fork'u, o beklentiyi karşılıksız bırakır.

Apache-2.0 yine de yarı yolda duruyor ve MPL-2.0 ile **uyumlu**:
MPL-2.0 kodu Apache-2.0 kodu ile aynı projede yaşayabilir. Yani
Apache'yi seçmemek, Apache ekosisteminden kopmak anlamına gelmiyor.

### GPLv3 — koruma güçlü, ama pazar kapanıyor

GPLv3 üç sorudan ikisini MPL'den **daha iyi** cevaplıyor: koruma
dosya değil sistem düzeyinde, patent hibesi daha geniş, ve tivoization
karşıtı maddeleri var.

Reddedilmesinin sebebi teknik değil, **pazar**: GPLv3 gömülü ve
savunma sektörünün kapısını kapatır. O sektörlerde ürünün tamamını
açmak çoğu zaman sözleşmeyle imkânsız, ve GPLv3'ün bağlantı kuralları
"tamamı" demeye yakın.

Ve o pazar, bu mimarinin **en gerçekçi ticari karşılığı.** Bir
capability tabanlı mikroçekirdeğin ayırt edici özelliği — küçük TCB,
yetki sınırları, bileşen izolasyonu — tam olarak sertifikasyon
gerektiren sistemlerin aradığı şey. seL4'ün gittiği yer de orası.

GPLv3 seçmek, projeyi en çok işe yarayacağı yerde kullanılamaz
kılardı. Koruma uğruna kullanıcıyı kaybetmek, korunacak bir şey
bırakmaz.

> **AGPLv3** ayrıca değerlendirilmedi çünkü sorunu bu proje için
> yok: AGPL'in eklediği şey ağ üzerinden *hizmet veren* yazılımın
> kaynağını açma zorunluluğu. Bir işletim sistemi çekirdeği o
> anlamda hizmet vermiyor.

### BSD-2-Clause — mevcut SPDX satırı, ve bir karar değildi

Yetmiş bir dosyada yazılıydı. Kaynağı ilk dosyanın şablonu; hiçbir
ADR'de tartışılmadı, hiçbir yerde gerekçelendirilmedi, ve kökte
`LICENSE` dosyası olmadığı için hukuken de eksikti.

Apache/MIT'ye yapılan itirazın aynısı geçerli — çekirdek
kapatılabilir — artı patent hibesi yok. **Bu ADR onu düzeltiyor**,
ve düzeltmenin kendisi bir bulgu: on dört milestone boyunca her
dosyanın başında duran bir satır, kimsenin verdiği bir karar
değildi.

Aynı şekil [ADR 023](023-test-disiplini.md)'ün K10'unda var: var
olmayan bir mekanizmayı tarif eden bir ADR ile, hiç alınmamış bir
kararı ilan eden bir SPDX satırı aynı şeyin iki hâli. İkisi de
doğru *görünüyor* ve ilk sorgulandığında dağılıyor.

## Sonuçlar

- `LICENSE` kökte, MPL-2.0'ın tam metni
  (<https://www.mozilla.org/MPL/2.0/>, 373 satır). Elle yazılmadı,
  kaynağından indirildi — bir hukuk metninde "aşağı yukarı doğru"
  diye bir şey yok.
- 72 kaynak dosyada `SPDX-License-Identifier: MPL-2.0`.
- Beş `Cargo.toml`'da `license = "MPL-2.0"`.
- **Korumanın sınırı dosya.** Bu depodaki bir dosyayı değiştiren
  paylaşır; yanına yeni dosya koyan koymaz.

### Kapsam dışı bırakılan tek dosya

`scripts/m9-index-probe/probe.s` **derleyici çıktısı**, elle
yazılmış kaynak değil — başında `.file "lib...cgu.0"` var. Ona
lisans başlığı eklemek, yazılmamış bir şeyin telifini iddia etmek
olurdu. Dosya M9'un bir ölçüm artığı ve girdisi (`src/lib.rs`)
lisanslı.

### Üçüncü taraf kod

`third_party/limine/` vendor edilmiş ve **kendi lisansı altında**
(BSD-2-Clause). MPL-2.0 ona dokunmuyor; ayrı bir eserin ayrı bir
lisansı. `make` ile çekiliyor ve `.gitignore`'da, yani bu depoda
dağıtılmıyor bile.

`limine` crate'i ve `bitflags` birer bağımlılık; lisansları kendi
paketlerinde.