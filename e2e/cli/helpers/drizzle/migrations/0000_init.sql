CREATE TABLE `authors` (
	`id` integer PRIMARY KEY AUTOINCREMENT NOT NULL,
	`name` text NOT NULL
);
--> statement-breakpoint
CREATE TABLE `posts` (
	`id` integer PRIMARY KEY AUTOINCREMENT NOT NULL,
	`author_id` integer NOT NULL REFERENCES `authors`(`id`) ON DELETE cascade,
	`title` text NOT NULL
);
--> statement-breakpoint
CREATE INDEX `posts_author_idx` ON `posts` (`author_id`);
