# ADR 004 — WSL Ubuntu build ortamı

Status: accepted (M0)

## Bağlam

Geliştirme makinesi Windows 11. Build zinciri şunlara ihtiyaç duyuyor:

- `rustup`/`cargo` nightly + `rust-src` (`-Z build-std` için)
- ELF çıktısı üretebilen bir linker — hedefin kendi `rust-lld`'si bunu
  sağlıyor, yani bu aslında Windows'ta da çalışırdı
- `xorriso` — hibrit BIOS/UEFI ISO üretimi
- `qemu-system-x86_64` — kabul testleri
- POSIX kabuk, `make`, `sed` — build akışı

Windows tarafında yapılan envanter:

| Araç | Durum |
|---|---|
| MSYS2 (`pacman`) | var |
| `xorriso` | MSYS2'de paket olarak var |
| `qemu-system-x86_64` | **yok**; MSYS2 deposunda da yok |
| Chocolatey | var, ama yönetici hakkı istiyor |
| `winget` | yok |
| WSL Ubuntu 2 | var |

Tıkanma noktası QEMU: MSYS2 bir `qemu-system-x86` paketi sunmuyor ve sistemde
kurulu bir QEMU yok. Chocolatey ile kurmak yükseltilmiş yetki gerektiriyor.

## Seçim

**Build ve testler WSL Ubuntu içinde çalışır.** Gerekli paketlerin tamamı tek
bir depodan geliyor:

```sh
sudo apt-get install -y build-essential xorriso qemu-system-x86 python3 git
```

Rust ayrıca `rustup` ile kuruluyor; `rust-toolchain.toml` kanalı, bileşenleri
ve hedefi sabitliyor, böylece ilk build gerekli her şeyi kendi çekiyor.

Kaynak ağacı Windows tarafında (`C:\Users\user\pyth`) durur ve WSL'den
`/mnt/c/...` üzerinden erişilir; düzenleme Windows araçlarıyla, build WSL'de.

`make toolchain-check` eksik araçları ve dağıtıma özel kurulum komutlarını
raporlar, böylece yeni bir geliştirici neyin eksik olduğunu tahmin etmez.

## Elenen alternatifler

**MSYS2'de native Windows build.** QEMU paketi yok. QEMU'yu ayrıca kurup
PATH'e eklemek mümkün olurdu, ama o zaman build ortamı iki ayrı kaynaktan
beslenir ve "çalışması için ne kurmalıyım" sorusunun cevabı belgelenmesi zor
hale gelir.

**Chocolatey ile QEMU kurmak.** Yönetici hakkı gerekiyor; ayrıca `xorriso`
tarafı yine MSYS2'de kalırdı — iki paket yöneticisi, iki ayrı güncelleme
yolu.

**Docker container.** Tekrarlanabilirlik açısından en iyisi ve ileride CI için
muhtemelen bu yöne gideceğiz. Ama Docker Desktop zaten WSL2 üzerinde çalışıyor,
yani şu an sadece bir soyutlama katmanı daha ekler. QEMU'yu container içinden
çalıştırmak KVM erişimi açısından ek yapılandırma ister.

**Sanal makine.** WSL2 zaten bir hafif VM; ayrıca tam VM, dosya paylaşımını ve
düzenleme akışını zorlaştırır.

## Sonuç

- Tek komutla tam tur: `wsl -d Ubuntu -- bash -lc "cd /mnt/c/Users/user/pyth
  && make test"`.
- Araç zinciri tek bir paket yöneticisinden geliyor; `make toolchain-check`
  eksikleri söylüyor.
- Düzenleme Windows'ta, build WSL'de — dosya sistemi sınırını geçmek
  `/mnt/c` üzerinden yavaş; `target/` dizini bu yüzden gözle görülür derecede
  yavaş yazılıyor. Build süreleri (~18 s tam derleme) kabul edilebilir
  seviyede, ama ileride `target/`'ı WSL'in kendi dosya sistemine taşımak bir
  iyileştirme seçeneği.
- Build'in Linux'a bağlılığı yalnızca `xorriso` + `qemu` + POSIX kabuk
  düzeyinde; Rust tarafı platformdan bağımsız. İleride native Windows'ta ISO
  üretmek istenirse yalnızca bu üçünün karşılığı bulunmalı.
- CI'ya geçerken bu ADR'nin yerini bir container tanımı alacak; o zaman bu
  kayıt "neden Docker'dan başlamadık" sorusunun cevabı olarak kalır.