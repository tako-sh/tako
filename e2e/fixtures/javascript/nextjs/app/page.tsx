import Image from "next/image";

export default function HomePage() {
  return (
    <main>
      <h1>Tako Next app</h1>
      <p>This fixture validates Next.js standalone deploys through tako.sh/nextjs.</p>
      <p>
        Public asset: <a href="/tako-mark.txt">/tako-mark.txt</a>
      </p>
      <Image
        src="/images/titan-yard.jpg"
        width={1200}
        height={676}
        sizes="(min-width: 1200px) 1200px, 100vw"
        alt="Titan Yard"
        priority
      />
    </main>
  );
}
