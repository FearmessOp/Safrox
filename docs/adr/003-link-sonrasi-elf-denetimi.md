# ADR 003 — Link sonrası ELF denetimi

Status: accepted (M0)

## Bağlam

Bir kernel'in boot etmemesinin en can sıkıcı biçimi, hiçbir şey söylemeden
boot etmemesidir: QEMU açılır, seri port bomboş kalır, hiçbir hata mesajı
yoktur. Bu noktada elinizde yalnızca `gdb` ve sabır vardır.

M0 sırasında bu tam olarak başımıza geldi. `.cargo/config.toml`'daki linker
script `RUSTFLAGS` ortam değişkeni tarafından ezildi; `cargo build` başarıyla
tamamlandı, ama ortaya çıkan ELF'in entry point'i `0x0` idi ve bölümler
adres 0x210'a yerleşmişti. Derleyici memnundu, linker memnundu, tek şikâyet
eden QEMU'nun sessizliği oldu.

Bu sınıftan hataların ortak özelliği şudur: **build başarılı görünür, hata
yalnızca çalışma anında ve sessizce ortaya çıkar.** Tipik sebepler:

- Linker script uygulanmamış → entry higher-half değil
- `--gc-sections` `.limine_requests`'i atmış → tüm response'lar `None`
- `PT_LOAD` segmentlerinin hizası veya dosya offset'i bozuk → Limine reddeder
- Mutlak build yolları binary'e sızmış → panic çıktısı makineye göre değişir

## Seçim

Her link'ten sonra, ISO üretilmeden önce `scripts/check_kernel.py` çalışır ve
ELF'i ham baytlardan okuyup şunları doğrular:

1. ELF sınıfı, endianness, makine (x86-64), tip
2. Entry point `0xffffffff80000000` veya üstünde
3. Her `PT_LOAD` segmenti: higher-half'ta, ≥4 KiB hizalı, dosya offset'i ile
   sayfa boyutuna göre denk (`(vaddr - offset) % 4096 == 0`), `memsz ≥ filesz`
4. En az bir `PT_LOAD` var
5. `.limine_requests` bölümü var ve boş değil
6. Mutlak build yolu (`/home/...`, `/mnt/c/...`) binary'de yok

Başarısızlık durumunda build durur ve hata, nereye bakılacağını söyler.
Makefile'da `$(KERNEL)` hedefinin son adımıdır, yani atlanması mümkün değil.

Script harici bağımlılık kullanmaz — `struct` ile ELF başlıklarını okur.
`readelf` çağırmak daha kısa olurdu ama çıktı biçimi binutils sürümüne göre
değişir ve ayrıştırması kırılgandır.

## Elenen alternatifler

**Hiç denetim yapmamak, QEMU'ya güvenmek.** Zaten bu yüzden bir saat
kaybettik. QEMU'nun verdiği geri bildirim "hiçbir şey olmadı"dır.

**`readelf`/`objdump` çıktısını grep'lemek.** binutils sürümleri arasında
çıktı biçimi değişiyor; ayrıca `llvm-readelf` ile `binutils readelf` farklı
yazıyor. Ham ELF okumak hem daha kısa hem daha kararlı.

**Linker script içinde `ASSERT()` kullanmak.** Entry adresi için işe yarardı,
ama `.limine_requests`'in boş olmadığını veya build yolu sızıntısını linker
script'ten kontrol edemeyiz. Yine de tamamlayıcı olarak ileride eklenebilir.

**Denetimi yalnızca CI'da çalıştırmak.** Hatanın en pahalı olduğu an, onu
yapan geliştiricinin önünde olmadığı andır. Her build'de çalışması, geri
bildirim döngüsünü saniyelere indiriyor.

## Sonuç

- Boot etmeyen bir ISO üretmek artık zor: yukarıdaki altı hatadan biri varsa
  build, ISO aşamasına hiç gelmez.
- Hata mesajları tanısal: "entry point 0x0 is below 0xffffffff80000000; the
  kernel is not higher-half (check code-model and linker.ld)".
- Build'e eklediği süre ölçülemeyecek kadar küçük (saf Python, tek dosya
  okuma).
- Mutlak yol denetimi, `trim-paths = "all"` ayarının gerçekten çalıştığının
  sürekli kanıtı; panic çıktısındaki `kernel/src/main.rs:236:9` her makinede
  aynı.
- Yeni bir sessiz-hata sınıfı keşfedildikçe buraya bir kontrol daha eklenir;
  script bu tür kurumsal bilginin biriktiği yer.