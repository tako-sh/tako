import Image from "next/image";

export default function HomePage() {
  return (
    <main>
      <h1>Tako Next.js Example</h1>
      <p>A minimal Next.js app deployed with Tako using the tako.sh/nextjs adapter.</p>
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
